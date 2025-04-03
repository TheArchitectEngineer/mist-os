// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::{FhoEnvironment, TryFromEnv};
use argh::{ArgsInfo, CommandInfo, FromArgs, SubCommand, SubCommands};
use async_trait::async_trait;
use ffx_command::{
    analytics_command, check_strict_constraints, send_enhanced_analytics, user_error, Error,
    FfxCommandLine, FfxContext, MetricsSession, Result, ToolRunner, ToolSuite,
};
use ffx_config::environment::ExecutableKind;
use ffx_config::EnvironmentContext;
use fho_metadata::FhoToolMetadata;
use std::fs::File;
use std::os::unix::process::ExitStatusExt;
use std::path::PathBuf;
use std::process::ExitStatus;
use writer::ToolIO;

/// The main trait for defining an ffx tool. This is not intended to be implemented directly
/// by the user, but instead derived via `#[derive(FfxTool)]`.
#[async_trait(?Send)]
pub trait FfxTool: FfxMain + Sized {
    type Command: FromArgs + SubCommand + ArgsInfo;

    fn supports_machine_output(&self) -> bool;
    fn has_schema(&self) -> bool;
    fn requires_target() -> bool;

    async fn from_env(env: FhoEnvironment, cmd: Self::Command) -> Result<Self>;

    /// Executes the tool. This is intended to be invoked by the user in main.
    async fn execute_tool() {
        let result = ffx_command::run::<FhoSuite<Self>>(ExecutableKind::Subtool).await;
        let should_format = match FfxCommandLine::from_env() {
            Ok(cli) => cli.global.machine.is_some(),
            Err(e) => {
                tracing::warn!("Received error getting command line: {}", e);
                match e {
                    Error::Help { .. } => false,
                    _ => true,
                }
            }
        };
        ffx_command::exit(result, should_format).await;
    }
}

#[async_trait(?Send)]
pub trait FfxMain: Sized {
    type Writer: TryFromEnv + ToolIO;

    /// The entrypoint of the tool. Once FHO has set up the environment for the tool, this is
    /// invoked. Should not be invoked directly unless for testing.
    async fn main(self, writer: Self::Writer) -> Result<()>;

    /// Given the writer, print the output schema. This is exposed to allow
    /// traversing the subtool adapters which combine more than one subtool which
    /// probably have different writers since they will have different output.
    async fn try_print_schema(self, mut writer: Self::Writer) -> Result<()> {
        writer.try_print_schema().map_err(|e| e.into())
    }

    /// Returns the basename of the log file to use with this tool. With the exception
    /// of long running tools, subtools are strongly encouraged to use the default basename.
    fn log_basename(&self) -> Option<String> {
        None
    }
}

#[derive(FromArgs)]
#[argh(subcommand)]
pub enum FhoHandler<M: FfxTool> {
    //FhoVersion1(M),
    /// Run the tool as if under ffx
    Standalone(M::Command),
    /// Print out the subtool's metadata json
    Metadata(MetadataCmd),
}

#[derive(Debug, FromArgs)]
#[argh(subcommand, name = "metadata", description = "Print out this subtool's FHO metadata json")]
pub struct MetadataCmd {
    #[argh(positional)]
    output_path: Option<PathBuf>,
}

#[derive(FromArgs)]
/// Fuchsia Host Objects Runner
pub struct ToolCommand<M: FfxTool> {
    #[argh(subcommand)]
    pub subcommand: FhoHandler<M>,
}

pub struct FhoSuite<M> {
    context: EnvironmentContext,
    _p: std::marker::PhantomData<fn(M) -> ()>,
}

impl<M> Clone for FhoSuite<M> {
    fn clone(&self) -> Self {
        Self { context: self.context.clone(), _p: self._p.clone() }
    }
}

struct FhoTool<M: FfxTool> {
    env: FhoEnvironment,
    redacted_args: Vec<String>,
    main: M,
}

struct MetadataRunner {
    cmd: MetadataCmd,
    info: &'static CommandInfo,
}

#[async_trait(?Send)]
impl ToolRunner for MetadataRunner {
    async fn run(self: Box<Self>, _metrics: MetricsSession) -> Result<ExitStatus> {
        // We don't ever want to emit metrics for a metadata query, it's a tool-level
        // command
        let meta = FhoToolMetadata::new(self.info.name, self.info.description);
        match &self.cmd.output_path {
            Some(path) => serde_json::to_writer_pretty(
                &File::create(path).with_user_message(|| {
                    format!("Failed to create metadata file {}", path.display())
                })?,
                &meta,
            ),
            None => serde_json::to_writer_pretty(&std::io::stdout(), &meta),
        }
        .user_message("Failed writing metadata")?;
        Ok(ExitStatus::from_raw(0))
    }
}

#[async_trait(?Send)]
impl<T: FfxTool> ToolRunner for FhoTool<T> {
    async fn run(self: Box<Self>, metrics: MetricsSession) -> Result<ExitStatus> {
        if !analytics_command(&self.redacted_args.join(" ")) {
            metrics.print_notice(&mut std::io::stderr()).await?;
        }
        let writer = TryFromEnv::try_from_env(&self.env).await?;
        let res: Result<ExitStatus> = if self.env.ffx_command().global.schema {
            if self.main.has_schema() {
                self.main
                    .try_print_schema(writer)
                    .await
                    .map(|_| ExitStatus::from_raw(0))
                    .map_err(|e| e.into())
            } else {
                Err(user_error!("--schema is not supported for this command (subtool)."))
            }
        } else {
            self.main.main(writer).await.map(|_| ExitStatus::from_raw(0))
        };
        let res = metrics.command_finished(&res, &self.redacted_args).await.and(res);
        self.env.maybe_wrap_connection_errors(res).await
    }
}

impl<T: FfxTool> FhoTool<T> {
    async fn build(
        context: &EnvironmentContext,
        ffx: FfxCommandLine,
        tool: T::Command,
    ) -> Result<Box<Self>> {
        check_strict_constraints(&ffx.global, T::requires_target())?;

        let is_machine_output = ffx.global.machine.is_some();
        let env = FhoEnvironment::new(context, &ffx);
        let redacted_args = match send_enhanced_analytics().await {
            false => ffx.redact_subcmd(&tool),
            true => ffx.unredacted_args_for_analytics(),
        };
        let main = T::from_env(env.clone(), tool).await?;
        if !main.supports_machine_output() && is_machine_output {
            return Err(Error::User(anyhow::anyhow!(
                "The machine flag is not supported for this subcommand"
            )));
        }

        let found = FhoTool { env, redacted_args, main };
        Ok(Box::new(found))
    }
}

#[async_trait::async_trait(?Send)]
impl<M: FfxTool> ToolSuite for FhoSuite<M> {
    async fn from_env(context: &EnvironmentContext) -> Result<Self> {
        let context = context.clone();
        Ok(Self { context: context, _p: Default::default() })
    }

    fn global_command_list() -> &'static [&'static argh::CommandInfo] {
        FhoHandler::<M>::COMMANDS
    }

    async fn get_args_info(&self) -> Result<ffx_command::CliArgsInfo> {
        Ok(M::Command::get_args_info().into())
    }

    async fn try_from_args(
        &self,
        ffx: &FfxCommandLine,
    ) -> Result<Option<Box<dyn ToolRunner + '_>>> {
        let args = Vec::from_iter(ffx.global.subcommand.iter().map(String::as_str));
        let command = ToolCommand::<M>::from_args(&Vec::from_iter(ffx.cmd_iter()), &args)
            .map_err(|err| Error::from_early_exit(&ffx.command, err))?;

        let res: Box<dyn ToolRunner> = match command.subcommand {
            FhoHandler::Metadata(cmd) => {
                Box::new(MetadataRunner { cmd, info: M::Command::COMMAND })
            }
            FhoHandler::Standalone(tool) => {
                FhoTool::<M>::build(&self.context, ffx.clone(), tool).await?
            }
        };
        Ok(Some(res))
    }

    async fn try_runner_from_name(
        &self,
        ffx: &FfxCommandLine,
    ) -> Result<Option<Box<dyn ToolRunner + '_>>> {
        let args = Vec::from_iter(ffx.global.subcommand.iter().map(String::as_str));
        match ToolCommand::<M>::from_args(&Vec::from_iter(ffx.cmd_iter()), &args) {
            Ok(cmd) => {
                let res: Box<dyn ToolRunner> = match cmd.subcommand {
                    FhoHandler::Metadata(cmd) => {
                        Box::new(MetadataRunner { cmd, info: M::Command::COMMAND })
                    }
                    FhoHandler::Standalone(tool) => {
                        FhoTool::<M>::build(&self.context, ffx.clone(), tool).await?
                    }
                };
                return Ok(Some(res));
            }
            Err(err) => {
                return Err(Error::from_early_exit(&ffx.command, err));
            }
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::tests::SimpleCheck;
    // This keeps the macros from having compiler errors.
    use crate::adapters::tests::{FakeCommand, FakeTool, TestWriter, SIMPLE_CHECK_COUNTER};
    use crate::{self as fho};
    use async_trait::async_trait;
    use fho_macro::FfxTool;
    use fho_metadata::{FhoDetails, Only};

    // The main testing part will happen in the `main()` function of the tool.
    #[fuchsia::test]
    async fn test_run_fake_tool() {
        let config_env = ffx_config::test_init().await.unwrap();
        let ffx = FfxCommandLine::new(None, &["ffx", "fake", "stuff"]).expect("test ffx cmd");
        let fho_env = FhoEnvironment::new(&config_env.context, &ffx);
        let writer = TestWriter;
        let fake_tool: FakeTool = build_tool(fho_env).await.expect("build fake tool");
        assert_eq!(
            SIMPLE_CHECK_COUNTER.with(|counter| *counter.borrow()),
            1,
            "tool pre-check should have been called once"
        );
        fake_tool.main(writer).await.unwrap();
    }

    #[fuchsia::test]
    async fn negative_precheck_fails() {
        #[derive(Debug, FfxTool)]
        #[check(SimpleCheck(false))]
        struct FakeToolWillFail {
            #[command]
            _fake_command: FakeCommand,
        }
        #[async_trait(?Send)]
        impl FfxMain for FakeToolWillFail {
            type Writer = TestWriter;
            async fn main(self, _writer: Self::Writer) -> Result<()> {
                panic!("This should never get called")
            }
        }

        let config_env = ffx_config::test_init().await.unwrap();
        let ffx = FfxCommandLine::new(None, &["ffx", "fake", "stuff"]).expect("test ffx cmd");
        let fho_env = FhoEnvironment::new(&config_env.context, &ffx);

        build_tool::<FakeToolWillFail>(fho_env)
            .await
            .expect_err("Should not have been able to create tool with a negative pre-check");
        assert_eq!(
            SIMPLE_CHECK_COUNTER.with(|counter| *counter.borrow()),
            1,
            "tool pre-check should have been called once"
        );
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn present_metadata() {
        let test_env = ffx_config::test_init().await.expect("Test env initialization failed");
        let tmpdir = tempfile::tempdir().expect("tempdir");

        let output_path = tmpdir.path().join("metadata.json");
        let cmd = MetadataCmd { output_path: Some(output_path.clone()) };
        let tool = Box::new(MetadataRunner { cmd, info: FakeCommand::COMMAND });
        let metrics = MetricsSession::start(&test_env.context).await.expect("Session start");

        tool.run(metrics).await.expect("running metadata command");

        let read_metadata: FhoToolMetadata =
            serde_json::from_reader(File::open(output_path).expect("opening metadata"))
                .expect("parsing metadata");
        assert_eq!(
            read_metadata,
            FhoToolMetadata {
                name: "fake".to_owned(),
                description: "fake command".to_owned(),
                requires_fho: 0,
                fho_details: FhoDetails::FhoVersion0 { version: Only },
            }
        );
    }

    pub async fn build_tool<T: FfxTool>(env: FhoEnvironment) -> Result<T> {
        let tool_cmd = ToolCommand::<T>::from_args(
            &Vec::from_iter(env.ffx_command().cmd_iter()),
            &Vec::from_iter(env.ffx_command().subcmd_iter()),
        )
        .unwrap();
        let fho::subtool::FhoHandler::Standalone(cmd) = tool_cmd.subcommand else {
            panic!("Not testing metadata generation");
        };
        T::from_env(env, cmd).await
    }
}
