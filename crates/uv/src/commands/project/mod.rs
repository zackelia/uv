use std::fmt::Write;

use itertools::Itertools;
use owo_colors::OwoColorize;
use tracing::debug;

use distribution_types::{Resolution, UnresolvedRequirementSpecification};
use pep440_rs::Version;
use pypi_types::Requirement;
use uv_cache::Cache;
use uv_client::{BaseClientBuilder, Connectivity, FlatIndexClient, RegistryClientBuilder};
use uv_configuration::{
    Concurrency, ExtrasSpecification, PreviewMode, Reinstall, SetupPyStrategy, Upgrade,
};
use uv_dispatch::BuildDispatch;
use uv_distribution::DistributionDatabase;
use uv_fs::Simplified;
use uv_installer::{SatisfiesResult, SitePackages};
use uv_python::{
    request_from_version_file, EnvironmentPreference, Interpreter, PythonEnvironment, PythonFetch,
    PythonInstallation, PythonPreference, PythonRequest, VersionRequest,
};
use uv_requirements::{NamedRequirementsResolver, RequirementsSpecification};
use uv_resolver::{
    FlatIndex, OptionsBuilder, PythonRequirement, RequiresPython, ResolutionGraph, ResolverMarkers,
};
use uv_types::{BuildIsolation, EmptyInstalledPackages, HashStrategy};
use uv_warnings::warn_user;
use uv_workspace::Workspace;

use crate::commands::pip::operations::Modifications;
use crate::commands::reporters::{PythonDownloadReporter, ResolverReporter};
use crate::commands::{pip, SharedState};
use crate::printer::Printer;
use crate::settings::{InstallerSettingsRef, ResolverInstallerSettings, ResolverSettingsRef};

pub(crate) mod add;
pub(crate) mod environment;
pub(crate) mod init;
pub(crate) mod lock;
pub(crate) mod remove;
pub(crate) mod run;
pub(crate) mod sync;
pub(crate) mod tree;

#[derive(thiserror::Error, Debug)]
pub(crate) enum ProjectError {
    #[error("The lockfile at `uv.lock` needs to be updated, but `--locked` was provided. To update the lockfile, run `uv lock`.")]
    LockMismatch,

    #[error(
        "Unable to find lockfile at `uv.lock`. To create a lockfile, run `uv lock` or `uv sync`."
    )]
    MissingLockfile,

    #[error("The current Python version ({0}) is not compatible with the locked Python requirement: `{1}`")]
    LockedPythonIncompatibility(Version, RequiresPython),

    #[error("The requested Python interpreter ({0}) is incompatible with the project Python requirement: `{1}`")]
    RequestedPythonIncompatibility(Version, RequiresPython),

    #[error(transparent)]
    Python(#[from] uv_python::Error),

    #[error(transparent)]
    Virtualenv(#[from] uv_virtualenv::Error),

    #[error(transparent)]
    HashStrategy(#[from] uv_types::HashStrategyError),

    #[error(transparent)]
    Tags(#[from] platform_tags::TagsError),

    #[error(transparent)]
    FlatIndex(#[from] uv_client::FlatIndexError),

    #[error(transparent)]
    Lock(#[from] uv_resolver::LockError),

    #[error(transparent)]
    Fmt(#[from] std::fmt::Error),

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    Anyhow(#[from] anyhow::Error),

    #[error(transparent)]
    Operation(#[from] pip::operations::Error),

    #[error(transparent)]
    RequiresPython(#[from] uv_resolver::RequiresPythonError),
}

/// Compute the `Requires-Python` bound for the [`Workspace`].
///
/// For a [`Workspace`] with multiple packages, the `Requires-Python` bound is the union of the
/// `Requires-Python` bounds of all the packages.
pub(crate) fn find_requires_python(
    workspace: &Workspace,
) -> Result<Option<RequiresPython>, uv_resolver::RequiresPythonError> {
    RequiresPython::union(workspace.packages().values().filter_map(|member| {
        member
            .pyproject_toml()
            .project
            .as_ref()
            .and_then(|project| project.requires_python.as_ref())
    }))
}

/// Find the virtual environment for the current project.
fn find_environment(
    workspace: &Workspace,
    cache: &Cache,
) -> Result<PythonEnvironment, uv_python::Error> {
    PythonEnvironment::from_root(workspace.venv(), cache)
}

/// Check if the given interpreter satisfies the project's requirements.
fn interpreter_meets_requirements(
    interpreter: &Interpreter,
    requested_python: Option<&PythonRequest>,
    cache: &Cache,
) -> bool {
    let Some(request) = requested_python else {
        return true;
    };
    if request.satisfied(interpreter, cache) {
        debug!("Interpreter meets the requested Python: `{request}`");
        true
    } else {
        debug!("Interpreter does not meet the request: `{request}`");
        false
    }
}

#[derive(Debug)]
pub(crate) enum FoundInterpreter {
    Interpreter(Interpreter),
    Environment(PythonEnvironment),
}

impl FoundInterpreter {
    /// Discover the interpreter to use in the current [`Workspace`].
    pub(crate) async fn discover(
        workspace: &Workspace,
        python_request: Option<PythonRequest>,
        python_preference: PythonPreference,
        python_fetch: PythonFetch,
        connectivity: Connectivity,
        native_tls: bool,
        cache: &Cache,
        printer: Printer,
    ) -> Result<Self, ProjectError> {
        let requires_python = find_requires_python(workspace)?;

        // (1) Explicit request from user
        let python_request = if let Some(request) = python_request {
            Some(request)
            // (2) Request from `.python-version`
        } else if let Some(request) = request_from_version_file().await? {
            Some(request)
            // (3) `Requires-Python` in `pyproject.toml`
        } else {
            requires_python
                .as_ref()
                .map(RequiresPython::specifiers)
                .map(|specifiers| PythonRequest::Version(VersionRequest::Range(specifiers.clone())))
        };

        // Read from the virtual environment first.
        match find_environment(workspace, cache) {
            Ok(venv) => {
                if interpreter_meets_requirements(
                    venv.interpreter(),
                    python_request.as_ref(),
                    cache,
                ) {
                    if let Some(requires_python) = requires_python.as_ref() {
                        if requires_python.contains(venv.interpreter().python_version()) {
                            return Ok(Self::Environment(venv));
                        }
                        debug!(
                            "Interpreter does not meet the project's Python requirement: `{requires_python}`"
                        );
                    } else {
                        return Ok(Self::Environment(venv));
                    }
                }
            }
            Err(uv_python::Error::MissingEnvironment(_)) => {}
            Err(uv_python::Error::Query(uv_python::InterpreterError::NotFound(path))) => {
                warn_user!(
                    "Ignoring existing virtual environment linked to non-existent Python interpreter: {}",
                    path.user_display().cyan()
                );
            }
            Err(err) => return Err(err.into()),
        };

        let client_builder = BaseClientBuilder::default()
            .connectivity(connectivity)
            .native_tls(native_tls);

        let reporter = PythonDownloadReporter::single(printer);

        // Locate the Python interpreter to use in the environment
        let interpreter = PythonInstallation::find_or_fetch(
            python_request,
            EnvironmentPreference::OnlySystem,
            python_preference,
            python_fetch,
            &client_builder,
            cache,
            Some(&reporter),
        )
        .await?
        .into_interpreter();

        writeln!(
            printer.stderr(),
            "Using Python {} interpreter at: {}",
            interpreter.python_version(),
            interpreter.sys_executable().user_display().cyan()
        )?;

        if let Some(requires_python) = requires_python.as_ref() {
            if !requires_python.contains(interpreter.python_version()) {
                return Err(ProjectError::RequestedPythonIncompatibility(
                    interpreter.python_version().clone(),
                    requires_python.clone(),
                ));
            }
        }

        Ok(Self::Interpreter(interpreter))
    }

    /// Convert the [`FoundInterpreter`] into an [`Interpreter`].
    pub(crate) fn into_interpreter(self) -> Interpreter {
        match self {
            FoundInterpreter::Interpreter(interpreter) => interpreter,
            FoundInterpreter::Environment(venv) => venv.into_interpreter(),
        }
    }
}

/// Initialize a virtual environment for the current project.
pub(crate) async fn get_or_init_environment(
    workspace: &Workspace,
    python: Option<PythonRequest>,
    python_preference: PythonPreference,
    python_fetch: PythonFetch,
    connectivity: Connectivity,
    native_tls: bool,
    cache: &Cache,
    printer: Printer,
) -> Result<PythonEnvironment, ProjectError> {
    match FoundInterpreter::discover(
        workspace,
        python,
        python_preference,
        python_fetch,
        connectivity,
        native_tls,
        cache,
        printer,
    )
    .await?
    {
        // If we found an existing, compatible environment, use it.
        FoundInterpreter::Environment(environment) => Ok(environment),

        // Otherwise, create a virtual environment with the discovered interpreter.
        FoundInterpreter::Interpreter(interpreter) => {
            let venv = workspace.venv();

            // Remove the existing virtual environment if it doesn't meet the requirements.
            match fs_err::remove_dir_all(&venv) {
                Ok(()) => {
                    writeln!(
                        printer.stderr(),
                        "Removed virtual environment at: {}",
                        venv.user_display().cyan()
                    )?;
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }

            writeln!(
                printer.stderr(),
                "Creating virtualenv at: {}",
                venv.user_display().cyan()
            )?;

            Ok(uv_virtualenv::create_venv(
                &venv,
                interpreter,
                uv_virtualenv::Prompt::None,
                false,
                false,
            )?)
        }
    }
}

/// Resolve any [`UnresolvedRequirementSpecification`] into a fully-qualified [`Requirement`].
pub(crate) async fn resolve_names(
    requirements: Vec<UnresolvedRequirementSpecification>,
    interpreter: &Interpreter,
    settings: &ResolverInstallerSettings,
    state: &SharedState,
    preview: PreviewMode,
    connectivity: Connectivity,
    concurrency: Concurrency,
    native_tls: bool,
    cache: &Cache,
    printer: Printer,
) -> anyhow::Result<Vec<Requirement>> {
    // Extract the project settings.
    let ResolverInstallerSettings {
        index_locations,
        index_strategy,
        keyring_provider,
        resolution: _,
        prerelease: _,
        config_setting,
        exclude_newer,
        link_mode,
        compile_bytecode: _,
        upgrade: _,
        reinstall: _,
        build_options,
    } = settings;

    // Initialize the registry client.
    let client = RegistryClientBuilder::new(cache.clone())
        .native_tls(native_tls)
        .connectivity(connectivity)
        .index_urls(index_locations.index_urls())
        .index_strategy(*index_strategy)
        .keyring(*keyring_provider)
        .markers(interpreter.markers())
        .platform(interpreter.platform())
        .build();

    // TODO(charlie): These are all default values. We should consider whether we want to make them
    // optional on the downstream APIs.
    let build_isolation = BuildIsolation::default();
    let hasher = HashStrategy::default();
    let setup_py = SetupPyStrategy::default();
    let flat_index = FlatIndex::default();

    // Create a build dispatch.
    let build_dispatch = BuildDispatch::new(
        &client,
        cache,
        interpreter,
        index_locations,
        &flat_index,
        &state.index,
        &state.git,
        &state.in_flight,
        *index_strategy,
        setup_py,
        config_setting,
        build_isolation,
        *link_mode,
        build_options,
        *exclude_newer,
        concurrency,
        preview,
    );

    // Initialize the resolver.
    let resolver = NamedRequirementsResolver::new(
        requirements,
        &hasher,
        &state.index,
        DistributionDatabase::new(&client, &build_dispatch, concurrency.downloads, preview),
    )
    .with_reporter(ResolverReporter::from(printer));

    Ok(resolver.resolve().await?)
}

/// Run dependency resolution for an interpreter, returning the [`ResolutionGraph`].
pub(crate) async fn resolve_environment<'a>(
    interpreter: &Interpreter,
    spec: RequirementsSpecification,
    settings: ResolverSettingsRef<'_>,
    state: &SharedState,
    preview: PreviewMode,
    connectivity: Connectivity,
    concurrency: Concurrency,
    native_tls: bool,
    cache: &Cache,
    printer: Printer,
) -> anyhow::Result<ResolutionGraph> {
    let ResolverSettingsRef {
        index_locations,
        index_strategy,
        keyring_provider,
        resolution,
        prerelease,
        config_setting,
        exclude_newer,
        link_mode,
        upgrade: _,
        build_options,
    } = settings;

    // Determine the tags, markers, and interpreter to use for resolution.
    let tags = interpreter.tags()?;
    let markers = interpreter.markers();
    let python_requirement = PythonRequirement::from_interpreter(interpreter);

    // Initialize the registry client.
    let client = RegistryClientBuilder::new(cache.clone())
        .native_tls(native_tls)
        .connectivity(connectivity)
        .index_urls(index_locations.index_urls())
        .index_strategy(index_strategy)
        .keyring(keyring_provider)
        .markers(markers)
        .platform(interpreter.platform())
        .build();

    let options = OptionsBuilder::new()
        .resolution_mode(resolution)
        .prerelease_mode(prerelease)
        .exclude_newer(exclude_newer)
        .index_strategy(index_strategy)
        .build();

    // TODO(charlie): These are all default values. We should consider whether we want to make them
    // optional on the downstream APIs.
    let build_isolation = BuildIsolation::default();
    let dev = Vec::default();
    let extras = ExtrasSpecification::default();
    let hasher = HashStrategy::default();
    let preferences = Vec::default();
    let setup_py = SetupPyStrategy::default();

    // When resolving from an interpreter, we assume an empty environment, so reinstalls and
    // upgrades aren't relevant.
    let reinstall = Reinstall::default();
    let upgrade = Upgrade::default();

    // Resolve the flat indexes from `--find-links`.
    let flat_index = {
        let client = FlatIndexClient::new(&client, cache);
        let entries = client.fetch(index_locations.flat_index()).await?;
        FlatIndex::from_entries(entries, Some(tags), &hasher, build_options)
    };

    // Create a build dispatch.
    let resolve_dispatch = BuildDispatch::new(
        &client,
        cache,
        interpreter,
        index_locations,
        &flat_index,
        &state.index,
        &state.git,
        &state.in_flight,
        index_strategy,
        setup_py,
        config_setting,
        build_isolation,
        link_mode,
        build_options,
        exclude_newer,
        concurrency,
        preview,
    );

    // Resolve the requirements.
    Ok(pip::operations::resolve(
        spec.requirements,
        spec.constraints,
        spec.overrides,
        dev,
        spec.source_trees,
        spec.project,
        &extras,
        preferences,
        EmptyInstalledPackages,
        &hasher,
        &reinstall,
        &upgrade,
        Some(tags),
        ResolverMarkers::SpecificEnvironment(markers.clone()),
        python_requirement,
        &client,
        &flat_index,
        &state.index,
        &resolve_dispatch,
        concurrency,
        options,
        printer,
        preview,
        false,
    )
    .await?)
}

/// Sync a [`PythonEnvironment`] with a set of resolved requirements.
pub(crate) async fn sync_environment(
    venv: PythonEnvironment,
    resolution: &Resolution,
    settings: InstallerSettingsRef<'_>,
    state: &SharedState,
    preview: PreviewMode,
    connectivity: Connectivity,
    concurrency: Concurrency,
    native_tls: bool,
    cache: &Cache,
    printer: Printer,
) -> anyhow::Result<PythonEnvironment> {
    let InstallerSettingsRef {
        index_locations,
        index_strategy,
        keyring_provider,
        config_setting,
        exclude_newer,
        link_mode,
        compile_bytecode,
        reinstall,
        build_options,
    } = settings;

    let site_packages = SitePackages::from_environment(&venv)?;

    // Determine the tags, markers, and interpreter to use for resolution.
    let interpreter = venv.interpreter();
    let tags = venv.interpreter().tags()?;
    let markers = venv.interpreter().markers();

    // Initialize the registry client.
    let client = RegistryClientBuilder::new(cache.clone())
        .native_tls(native_tls)
        .connectivity(connectivity)
        .index_urls(index_locations.index_urls())
        .index_strategy(index_strategy)
        .keyring(keyring_provider)
        .markers(markers)
        .platform(interpreter.platform())
        .build();

    // TODO(charlie): These are all default values. We should consider whether we want to make them
    // optional on the downstream APIs.
    let build_isolation = BuildIsolation::default();
    let dry_run = false;
    let hasher = HashStrategy::default();
    let setup_py = SetupPyStrategy::default();

    // Resolve the flat indexes from `--find-links`.
    let flat_index = {
        let client = FlatIndexClient::new(&client, cache);
        let entries = client.fetch(index_locations.flat_index()).await?;
        FlatIndex::from_entries(entries, Some(tags), &hasher, build_options)
    };

    // Create a build dispatch.
    let build_dispatch = BuildDispatch::new(
        &client,
        cache,
        interpreter,
        index_locations,
        &flat_index,
        &state.index,
        &state.git,
        &state.in_flight,
        index_strategy,
        setup_py,
        config_setting,
        build_isolation,
        link_mode,
        build_options,
        exclude_newer,
        concurrency,
        preview,
    );

    // Sync the environment.
    pip::operations::install(
        resolution,
        site_packages,
        Modifications::Exact,
        reinstall,
        build_options,
        link_mode,
        compile_bytecode,
        index_locations,
        &hasher,
        tags,
        &client,
        &state.in_flight,
        concurrency,
        &build_dispatch,
        cache,
        &venv,
        dry_run,
        printer,
        preview,
    )
    .await?;

    // Notify the user of any resolution diagnostics.
    pip::operations::diagnose_resolution(resolution.diagnostics(), printer)?;

    Ok(venv)
}

/// Update a [`PythonEnvironment`] to satisfy a set of [`RequirementsSource`]s.
pub(crate) async fn update_environment(
    venv: PythonEnvironment,
    spec: RequirementsSpecification,
    settings: &ResolverInstallerSettings,
    state: &SharedState,
    preview: PreviewMode,
    connectivity: Connectivity,
    concurrency: Concurrency,
    native_tls: bool,
    cache: &Cache,
    printer: Printer,
) -> anyhow::Result<PythonEnvironment> {
    let ResolverInstallerSettings {
        index_locations,
        index_strategy,
        keyring_provider,
        resolution,
        prerelease,
        config_setting,
        exclude_newer,
        link_mode,
        compile_bytecode,
        upgrade,
        reinstall,
        build_options,
    } = settings;

    // Check if the current environment satisfies the requirements
    let site_packages = SitePackages::from_environment(&venv)?;
    if spec.source_trees.is_empty() && reinstall.is_none() && upgrade.is_none() {
        match site_packages.satisfies(&spec.requirements, &spec.constraints)? {
            // If the requirements are already satisfied, we're done.
            SatisfiesResult::Fresh {
                recursive_requirements,
            } => {
                debug!(
                    "All requirements satisfied: {}",
                    recursive_requirements
                        .iter()
                        .map(|entry| entry.requirement.to_string())
                        .sorted()
                        .join(" | ")
                );
                return Ok(venv);
            }
            SatisfiesResult::Unsatisfied(requirement) => {
                debug!("At least one requirement is not satisfied: {requirement}");
            }
        }
    }

    // Determine the tags, markers, and interpreter to use for resolution.
    let interpreter = venv.interpreter();
    let tags = venv.interpreter().tags()?;
    let markers = venv.interpreter().markers();
    let python_requirement = PythonRequirement::from_interpreter(interpreter);

    // Initialize the registry client.
    let client = RegistryClientBuilder::new(cache.clone())
        .native_tls(native_tls)
        .connectivity(connectivity)
        .index_urls(index_locations.index_urls())
        .index_strategy(*index_strategy)
        .keyring(*keyring_provider)
        .markers(markers)
        .platform(interpreter.platform())
        .build();

    let options = OptionsBuilder::new()
        .resolution_mode(*resolution)
        .prerelease_mode(*prerelease)
        .exclude_newer(*exclude_newer)
        .index_strategy(*index_strategy)
        .build();

    // TODO(charlie): These are all default values. We should consider whether we want to make them
    // optional on the downstream APIs.
    let build_isolation = BuildIsolation::default();
    let dev = Vec::default();
    let dry_run = false;
    let extras = ExtrasSpecification::default();
    let hasher = HashStrategy::default();
    let preferences = Vec::default();
    let setup_py = SetupPyStrategy::default();

    // Resolve the flat indexes from `--find-links`.
    let flat_index = {
        let client = FlatIndexClient::new(&client, cache);
        let entries = client.fetch(index_locations.flat_index()).await?;
        FlatIndex::from_entries(entries, Some(tags), &hasher, build_options)
    };

    // Create a build dispatch.
    let build_dispatch = BuildDispatch::new(
        &client,
        cache,
        interpreter,
        index_locations,
        &flat_index,
        &state.index,
        &state.git,
        &state.in_flight,
        *index_strategy,
        setup_py,
        config_setting,
        build_isolation,
        *link_mode,
        build_options,
        *exclude_newer,
        concurrency,
        preview,
    );

    // Resolve the requirements.
    let resolution = match pip::operations::resolve(
        spec.requirements,
        spec.constraints,
        spec.overrides,
        dev,
        spec.source_trees,
        spec.project,
        &extras,
        preferences,
        site_packages.clone(),
        &hasher,
        reinstall,
        upgrade,
        Some(tags),
        ResolverMarkers::SpecificEnvironment(markers.clone()),
        python_requirement,
        &client,
        &flat_index,
        &state.index,
        &build_dispatch,
        concurrency,
        options,
        printer,
        preview,
        false,
    )
    .await
    {
        Ok(resolution) => Resolution::from(resolution),
        Err(err) => return Err(err.into()),
    };

    // Sync the environment.
    pip::operations::install(
        &resolution,
        site_packages,
        Modifications::Exact,
        reinstall,
        build_options,
        *link_mode,
        *compile_bytecode,
        index_locations,
        &hasher,
        tags,
        &client,
        &state.in_flight,
        concurrency,
        &build_dispatch,
        cache,
        &venv,
        dry_run,
        printer,
        preview,
    )
    .await?;

    // Notify the user of any resolution diagnostics.
    pip::operations::diagnose_resolution(resolution.diagnostics(), printer)?;

    Ok(venv)
}
