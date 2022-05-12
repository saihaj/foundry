//! Coverage command
use crate::{
    cmd::{
        forge::{build::CoreBuildArgs, test::Filter},
        Cmd,
    },
    compile::ProjectCompiler,
    utils,
};
use clap::{AppSettings, ArgEnum, Parser};
use ethers::{
    prelude::{Artifact, Project, ProjectCompileOutput},
    solc::{
        artifacts::contract::CompactContractBytecode,
        sourcemap::{self, SourceMap},
        ArtifactId,
    },
};
use forge::{
    coverage::{CoverageMap, CoverageReporter, LcovReporter, SummaryReporter, Visitor},
    executor::opts::EvmOpts,
    result::SuiteResult,
    trace::{identifier::LocalTraceIdentifier, CallTraceDecoder, CallTraceDecoderBuilder},
    MultiContractRunnerBuilder,
};
use foundry_common::evm::EvmArgs;
use foundry_config::{figment::Figment, Config};
use std::{collections::BTreeMap, sync::mpsc::channel, thread};

// Loads project's figment and merges the build cli arguments into it
foundry_config::impl_figment_convert!(CoverageArgs, opts, evm_opts);

/// Generate coverage reports for your tests.
#[derive(Debug, Clone, Parser)]
#[clap(global_setting = AppSettings::DeriveDisplayOrder)]
pub struct CoverageArgs {
    #[clap(
        long,
        arg_enum,
        default_value = "summary",
        help = "The report type to use for coverage."
    )]
    report: CoverageReportKind,

    #[clap(flatten, next_help_heading = "TEST FILTERING")]
    filter: Filter,

    #[clap(flatten, next_help_heading = "EVM OPTIONS")]
    evm_opts: EvmArgs,

    #[clap(flatten, next_help_heading = "BUILD OPTIONS")]
    opts: CoreBuildArgs,
}

impl CoverageArgs {
    /// Returns the flattened [`CoreBuildArgs`]
    pub fn build_args(&self) -> &CoreBuildArgs {
        &self.opts
    }

    /// Returns the currently configured [Config] and the extracted [EvmOpts] from that config
    pub fn config_and_evm_opts(&self) -> eyre::Result<(Config, EvmOpts)> {
        // merge all configs
        let figment: Figment = self.into();
        let evm_opts = figment.extract()?;
        let config = Config::from_provider(figment).sanitized();

        Ok((config, evm_opts))
    }
}

impl Cmd for CoverageArgs {
    type Output = ();

    fn run(self) -> eyre::Result<Self::Output> {
        let (config, evm_opts) = self.configure()?;
        let (project, output) = self.build(&config)?;
        println!("Analysing contracts...");
        let (map, source_maps) = self.prepare(output.clone())?;

        println!("Running tests...");
        self.collect(project, output, source_maps, map, config, evm_opts)
    }
}

impl CoverageArgs {
    /// Collects and adjusts configuration.
    fn configure(&self) -> eyre::Result<(Config, EvmOpts)> {
        // Merge all configs
        let (config, mut evm_opts) = self.config_and_evm_opts()?;

        // We always want traces
        evm_opts.verbosity = 3;

        Ok((config, evm_opts))
    }

    /// Builds the project.
    fn build(&self, config: &Config) -> eyre::Result<(Project, ProjectCompileOutput)> {
        // Set up the project
        let project = {
            let mut project = config.ephemeral_no_artifacts_project()?;

            // Disable the optimizer for more accurate source maps
            project.solc_config.settings.optimizer.disable();

            project
        };

        // TODO: This does not strip file prefixes for `SourceFiles`...
        let output = ProjectCompiler::default()
            .compile(&project)?
            .with_stripped_file_prefixes(project.root());

        Ok((project, output))
    }

    /// Builds the coverage map.
    fn prepare(
        &self,
        output: ProjectCompileOutput,
    ) -> eyre::Result<(CoverageMap, BTreeMap<ArtifactId, SourceMap>)> {
        // Get sources and source maps
        let (artifacts, sources) = output.into_artifacts_with_sources();

        let source_maps: BTreeMap<ArtifactId, SourceMap> = artifacts
            .into_iter()
            .filter(|(_, artifact)| {
                // TODO: Filter out dependencies
                // Filter out tests
                !artifact
                    .get_abi()
                    .map(|abi| abi.functions().any(|f| f.name.starts_with("test")))
                    .unwrap_or_default()
            })
            .map(|(id, artifact)| (id, CompactContractBytecode::from(artifact)))
            .filter_map(|(id, artifact): (ArtifactId, CompactContractBytecode)| {
                let source_map = artifact
                    .deployed_bytecode
                    .as_ref()
                    .and_then(|bytecode| bytecode.bytecode.as_ref())
                    .and_then(|bytecode| bytecode.source_map.as_ref())
                    .and_then(|source_map| sourcemap::parse(source_map).ok())?;

                Some((id, source_map))
            })
            .collect();

        let mut map = CoverageMap::new();
        for (path, versioned_sources) in sources.0.into_iter() {
            for mut versioned_source in versioned_sources {
                let source = &mut versioned_source.source_file;
                if let Some(ast) = source.ast.take() {
                    let mut visitor = Visitor::new();
                    visitor.visit_ast(ast)?;

                    if visitor.items.is_empty() {
                        continue
                    }

                    map.add_source(path.clone(), versioned_source, visitor.items);
                }
            }
        }

        Ok((map, source_maps))
    }

    /// Runs tests, collects coverage data and generates the final report.
    fn collect(
        self,
        project: Project,
        output: ProjectCompileOutput,
        _source_maps: BTreeMap<ArtifactId, SourceMap>,
        map: CoverageMap,
        config: Config,
        evm_opts: EvmOpts,
    ) -> eyre::Result<()> {
        // Setup the fuzzer
        // TODO: Add CLI Options to modify the persistence
        let cfg = proptest::test_runner::Config {
            failure_persistence: None,
            cases: config.fuzz_runs,
            max_local_rejects: config.fuzz_max_local_rejects,
            max_global_rejects: config.fuzz_max_global_rejects,
            ..Default::default()
        };
        let fuzzer = proptest::test_runner::TestRunner::new(cfg);
        let root = project.paths.root;

        // Build the contract runner
        let evm_spec = crate::utils::evm_spec(&config.evm_version);
        let mut runner = MultiContractRunnerBuilder::default()
            .fuzzer(fuzzer)
            .initial_balance(evm_opts.initial_balance)
            .evm_spec(evm_spec)
            .sender(evm_opts.sender)
            .with_fork(utils::get_fork(&evm_opts, &config.rpc_storage_caching))
            .with_coverage()
            .build(root.clone(), output, evm_opts)?;
        let (tx, rx) = channel::<(String, SuiteResult)>();

        // Set up identifier
        let local_identifier = LocalTraceIdentifier::new(&runner.known_contracts);

        // TODO: Coverage for fuzz tests
        let handle = thread::spawn(move || runner.test(&self.filter, Some(tx), false).unwrap());
        for mut result in rx.into_iter().flat_map(|(_, suite)| suite.test_results.into_values()) {
            if let Some(_hit_data) = result.coverage.take() {
                let mut decoder =
                    CallTraceDecoderBuilder::new().with_events(local_identifier.events()).build();
                for (_, trace) in &mut result.traces {
                    decoder.identify(trace, &local_identifier);
                }
                // TODO: We need an ArtifactId here for the addresses
                let CallTraceDecoder { contracts: _, .. } = decoder;

                // ..
            }
        }

        // Reattach the thread
        let _ = handle.join();

        match self.report {
            CoverageReportKind::Summary => {
                let mut reporter = SummaryReporter::new();
                reporter.build(map);
                reporter.finalize()
            }
            // TODO: Sensible place to put the LCOV file
            CoverageReportKind::Lcov => {
                let mut reporter =
                    LcovReporter::new(std::fs::File::create(root.join("lcov.info"))?);
                reporter.build(map);
                reporter.finalize()
            }
        }
    }
}

// TODO: HTML
#[derive(Debug, Clone, ArgEnum)]
pub enum CoverageReportKind {
    Summary,
    Lcov,
}

// TODO: Move reporters to own module