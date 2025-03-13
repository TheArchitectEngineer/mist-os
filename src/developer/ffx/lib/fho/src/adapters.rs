// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

/// Use this macro to use the new subtool interfaces in a plugin embedded in ffx (or
/// another subtool). It takes the type that implements `FfxTool` and `FfxMain` as an
/// argument and sets up the global functions that the old `#[plugin()]` interface
/// used to do.
#[macro_export]
macro_rules! embedded_plugin {
    ($tool:ty) => {
        pub async fn ffx_plugin_impl(
            env: &$crate::macro_deps::fho::FhoEnvironment,
            cmd: <$tool as $crate::FfxTool>::Command,
        ) -> $crate::Result<()> {
            #[allow(unused_imports)]
            use $crate::macro_deps::{
                argh, bug, check_strict_constraints, global_env_context, return_bug,
                writer::{Format, ToolIO},
            };
            use $crate::FfxMain as _;

            $crate::macro_deps::check_strict_constraints(
                &env.ffx_command().global,
                <$tool as $crate::FfxTool>::requires_target(),
            )?;

            // Create the writer, and if the schema is flag is set,
            // try to print it and then exit.
            let mut writer: <$tool as $crate::FfxMain>::Writer =
                $crate::TryFromEnv::try_from_env(&env).await?;
            if env.ffx_command().global.schema {
                if <<$tool as $crate::FfxMain>::Writer as ToolIO>::has_schema() {
                    writer.try_print_schema()?;
                } else {
                    $crate::macro_deps::return_user_error!(
                        "--schema is not supported for this command"
                    );
                }
                return Ok(());
            }

            let tool = <$tool as $crate::subtool::FfxTool>::from_env(env.clone(), cmd).await?;
            env.update_log_file(tool.log_basename())?;
            let res = $crate::FfxMain::main(tool, writer).await;
            // The env must not be dropped entirely until after the main function has completed, as
            // the underlying backing connection is kept inside the environment (in the case of
            // direct connections via `crate::connector::DefaultConnector`.
            env.maybe_wrap_connection_errors(res).await
        }

        pub fn ffx_plugin_is_machine_supported() -> bool {
            use $crate::macro_deps::writer::ToolIO;
            <<$tool as $crate::FfxMain>::Writer as ToolIO>::is_machine_supported()
        }

        pub fn ffx_plugin_has_schema() -> bool {
            use $crate::macro_deps::writer::ToolIO;
            <<$tool as $crate::FfxMain>::Writer as ToolIO>::has_schema()
        }
    };
}

#[cfg(test)]
pub(crate) mod tests {
    use crate::macro_deps::fho;
    use crate::subtool::{FhoHandler, ToolCommand};
    use crate::{CheckEnv, FfxMain, FfxTool, FhoEnvironment, TryFromEnv};
    use argh::{ArgsInfo, FromArgs};
    use async_trait::async_trait;
    use ffx_command::FfxCommandLine;
    use ffx_command_error::Result;
    use std::cell::RefCell;
    use writer::ToolIO;

    #[derive(Debug, ArgsInfo, FromArgs)]
    #[argh(subcommand, name = "fake", description = "fake command")]
    pub struct FakeCommand {
        #[argh(positional)]
        /// just needs a doc here so the macro doesn't complain.
        stuff: String,
    }

    thread_local! {
        pub static SIMPLE_CHECK_COUNTER: RefCell<u64> = RefCell::new(0);
    }

    pub struct SimpleCheck(pub bool);

    #[async_trait(?Send)]
    impl CheckEnv for SimpleCheck {
        async fn check_env(self, _env: &FhoEnvironment) -> Result<()> {
            SIMPLE_CHECK_COUNTER.with(|counter| *counter.borrow_mut() += 1);
            if self.0 {
                Ok(())
            } else {
                Err(anyhow::anyhow!("SimpleCheck was false").into())
            }
        }
    }

    #[derive(Debug)]
    pub struct NewTypeString(String);

    #[async_trait(?Send)]
    impl TryFromEnv for NewTypeString {
        async fn try_from_env(_env: &FhoEnvironment) -> Result<Self> {
            Ok(Self(String::from("foobar")))
        }
    }
    #[derive(FfxTool, Debug)]
    #[check(SimpleCheck(true))]
    pub struct FakeTool {
        from_env_string: NewTypeString,
        #[command]
        fake_command: FakeCommand,
    }

    pub struct TestWriter;

    #[async_trait(?Send)]
    impl TryFromEnv for TestWriter {
        async fn try_from_env(_env: &FhoEnvironment) -> Result<Self> {
            Ok(TestWriter)
        }
    }

    impl ToolIO for TestWriter {
        type OutputItem = String;

        fn is_machine_supported() -> bool {
            false
        }

        fn is_machine(&self) -> bool {
            false
        }

        fn stderr(&mut self) -> &'_ mut Box<dyn std::io::Write> {
            todo!()
        }

        fn item(&mut self, _value: &Self::OutputItem) -> writer::Result<()>
        where
            Self::OutputItem: std::fmt::Display,
        {
            todo!()
        }
    }
    impl std::io::Write for TestWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[async_trait(?Send)]
    impl FfxMain for FakeTool {
        type Writer = TestWriter;
        async fn main(self, mut writer: Self::Writer) -> Result<()> {
            assert_eq!(self.from_env_string.0, "foobar");
            assert_eq!(self.fake_command.stuff, "stuff");
            writer.line("junk-line").unwrap();
            Ok(())
        }
    }

    // The main testing part will happen in the `main()` function of the tool.
    #[fuchsia::test]
    async fn test_run_fake_tool_with_legacy_shim() {
        let config_env = ffx_config::test_init().await.expect("Initializing test environment");
        let ffx_cmd_line = FfxCommandLine::new(None, &["ffx", "fake", "stuff"]).unwrap();
        let tool_cmd = ToolCommand::<FakeTool>::from_args(
            &Vec::from_iter(ffx_cmd_line.cmd_iter()),
            &Vec::from_iter(ffx_cmd_line.subcmd_iter()),
        )
        .unwrap();

        embedded_plugin!(FakeTool);

        assert_eq!(
            SIMPLE_CHECK_COUNTER.with(|counter| *counter.borrow()),
            0,
            "tool pre-check should not have been called yet"
        );

        assert!(
            !ffx_plugin_is_machine_supported(),
            "Test plugin should not support machine output"
        );

        assert!(!ffx_plugin_has_schema(), "Test plugin should not support machine output schema");

        let fake_tool = match tool_cmd.subcommand {
            FhoHandler::Standalone(t) => t,
            FhoHandler::Metadata(_) => panic!("Not testing metadata generation"),
        };

        let env = FhoEnvironment::new(&config_env.context, &ffx_cmd_line);

        ffx_plugin_impl(&env, fake_tool).await.expect("Plugin to run successfully");

        assert_eq!(
            SIMPLE_CHECK_COUNTER.with(|counter| *counter.borrow()),
            1,
            "tool pre-check should have been called once"
        );
    }
}
