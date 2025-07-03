// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
mod args;

use anyhow::Result;
use args::{ProfilerCommand, ProfilerSubCommand};
use async_fs::File;
use core::fmt;
use errors::{ffx_bail, ffx_error};
use ffx_writer::{MachineWriter, ToolIO as _};
use fho::{deferred, FfxMain, FfxTool};
use log::info;
use schemars::JsonSchema;
use serde::Serialize;
use std::io::{stdin, BufRead};
use std::time::Duration;
use target_holders::moniker;
use tempfile::Builder;
use termion::{color, style};
use {fidl_fuchsia_cpu_profiler as profiler, fidl_fuchsia_test_manager as test_manager};

#[derive(Serialize, JsonSchema)]
pub struct ShowCpuProfilerCmd {
    pub samples_collected: Option<u64>,
    pub median_sample_time: Option<u64>,
    pub mean_sample_time: Option<u64>,
    pub max_sample_time: Option<u64>,
    pub min_sample_time: Option<u64>,
    pub missing_process_mappings: Option<Vec<u64>>,
}

impl fmt::Display for ShowCpuProfilerCmd {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Session Stats: \n")?;
        if let Some(num_samples) = self.samples_collected {
            write!(f, "    Number of samples collected: {}\n", num_samples)?;
        }
        if let Some(median_sample_time) = self.median_sample_time {
            write!(f, "    Median sample time: {}us\n", median_sample_time)?;
        }
        if let Some(mean_sample_time) = self.mean_sample_time {
            write!(f, "    Mean sample time: {}us\n", mean_sample_time)?;
        }
        if let Some(max_sample_time) = self.max_sample_time {
            write!(f, "    Max sample time: {}us\n", max_sample_time)?;
        }
        if let Some(min_sample_time) = self.min_sample_time {
            write!(f, "    Min sample time: {}us\n", min_sample_time)?;
        }
        if let Some(ref pids) = self.missing_process_mappings {
            write!(f, "    Processes missing mappings: {:?}\n", pids)?;
        }
        Ok(())
    }
}

type Writer = MachineWriter<ShowCpuProfilerCmd>;
#[derive(FfxTool)]
pub struct ProfilerTool {
    #[with(deferred(moniker("/core/profiler")))]
    controller: fho::Deferred<profiler::SessionProxy>,
    #[command]
    cmd: ProfilerCommand,
}

#[async_trait::async_trait(?Send)]
impl FfxMain for ProfilerTool {
    type Writer = Writer;

    async fn main(self, writer: Self::Writer) -> fho::Result<()> {
        info!(cmd:? = self.cmd; "Running profiler... ");
        Ok(profiler(self.controller, writer, self.cmd).await?)
    }
}

fn gather_targets(opts: &args::Attach) -> Result<fidl_fuchsia_cpu_profiler::TargetConfig> {
    if let Some(moniker) = &opts.moniker {
        if !opts.pids.is_empty()
            || !opts.tids.is_empty()
            || !opts.job_ids.is_empty()
            || opts.system_wide
        {
            ffx_bail!(
                "Targeting both a component and specific jobs/processes/threads is not supported"
            )
        }
        let component_config = profiler::AttachConfig::AttachToComponentMoniker(moniker.clone());
        Ok(profiler::TargetConfig::Component(component_config))
    } else if let Some(url) = &opts.url {
        if !opts.pids.is_empty()
            || !opts.tids.is_empty()
            || !opts.job_ids.is_empty()
            || opts.system_wide
        {
            ffx_bail!(
                "Targeting both a component and specific jobs/processes/threads is not supported"
            )
        }
        let component_config = profiler::AttachConfig::AttachToComponentUrl(url.clone());
        Ok(profiler::TargetConfig::Component(component_config))
    } else {
        let mut tasks: Vec<_> = opts
            .job_ids
            .iter()
            .map(|&id| profiler::Task::Job(id))
            .chain(opts.pids.iter().map(|&id| profiler::Task::Process(id)))
            .chain(opts.tids.iter().map(|&id| profiler::Task::Thread(id)))
            .collect();
        if opts.system_wide {
            tasks.push(profiler::Task::SystemWide(profiler::SystemWide {}));
        }
        if tasks.is_empty() {
            ffx_bail!("No targets were specified")
        }
        Ok(profiler::TargetConfig::Tasks(tasks))
    }
}

#[derive(Debug)]
struct SessionOpts {
    symbolize: bool,
    buffer_size_mb: Option<u64>,
    print_stats: bool,
    pprof_conversion: bool,
    output: String,
    duration: Option<u64>,
    color_output: bool,
}

async fn run_session(
    controller: fho::Deferred<profiler::SessionProxy>,
    mut writer: Writer,
    config: profiler::Config,
    opts: SessionOpts,
) -> Result<()> {
    info!(config:? = config, opts:? = opts; "Running profiler session...");
    let (client, server) = fidl::Socket::create_stream();
    let client = fidl::AsyncSocket::from_socket(client);
    let controller = controller.await?;
    controller
        .configure(profiler::SessionConfigureRequest {
            output: Some(server),
            config: Some(config),
            ..Default::default()
        })
        .await?
        .map_err(|e| ffx_error!("Failed to start: {:?}", e))?;
    info!("Profiler session is configured.");

    let tmp_dir = Builder::new().prefix("fuchsia_cpu_profiler_").tempdir()?;

    let unsymbolized_path = if opts.symbolize {
        tmp_dir.path().join("unsymbolized.txt")
    } else {
        std::path::PathBuf::from(&opts.output)
    };

    let mut output = File::create(&unsymbolized_path).await?;
    let copy_task =
        fuchsia_async::Task::local(async move { futures::io::copy(client, &mut output).await });

    info!("Starting profiler...");
    controller
        .start(&profiler::SessionStartRequest {
            buffer_results: Some(true),
            buffer_size_mb: opts.buffer_size_mb,
            ..Default::default()
        })
        .await?
        .map_err(|e| ffx_error!("Failed to start: {:?}", e))?;
    info!("Profiler started.");

    if let &Some(duration) = &opts.duration {
        writer.line(format!("Waiting for {} seconds...", duration))?;
        fuchsia_async::Timer::new(Duration::from_secs(duration)).await;
    } else {
        writer.line("Press <enter> to stop profiling...")?;
        blocking::unblock(|| {
            let _ = stdin().lock().read_line(&mut String::new());
        })
        .await;
    }
    info!("Stopping profiler...");
    let stats = controller.stop().await?;
    if let Some(ref pids) = &stats.missing_process_mappings {
        if !pids.is_empty() {
            writeln!(
                writer.stderr(),
                "{}[WARNING] Failed to get symbols for some processes: {:?}\n\
                This can occur when processes exit before the profiler is able to read their modules.{}",
                if opts.color_output {
                    format!("{}", color::Fg(color::Red))
                } else {
                    String::from("")
                },
                pids,
                if opts.color_output { format!("{}", style::Reset) } else { String::from("") },
            )?;
        }
    }
    if opts.print_stats {
        let output = ShowCpuProfilerCmd {
            samples_collected: stats.samples_collected,
            median_sample_time: stats.median_sample_time,
            mean_sample_time: stats.mean_sample_time,
            max_sample_time: stats.max_sample_time,
            min_sample_time: stats.min_sample_time,
            missing_process_mappings: stats.missing_process_mappings,
        };
        writer.machine(&output)?;
        writer.line(format!("\n{output}"))?;
    }
    info!("Profiler stopped, waiting for copy to complete...");
    copy_task.await?;
    info!("Copy from profiler completed, resetting profiler...");
    controller.reset().await?;
    info!("Profiler state reset.");

    if !opts.symbolize {
        return Ok(());
    }
    if let Ok(symbolized_record) = ffx_profiler::symbolize::symbolize(
        &unsymbolized_path,
        &opts.output.clone().into(),
        opts.pprof_conversion,
    ) {
        return ffx_profiler::pprof::samples_to_pprof(symbolized_record, opts.output.into());
    } else {
        anyhow::bail!("Failed to symbolize profile");
    }
}

pub async fn profiler(
    controller: fho::Deferred<profiler::SessionProxy>,
    writer: Writer,
    cmd: ProfilerCommand,
) -> Result<()> {
    let (targets, config, session_opts) = match cmd.sub_cmd {
        ProfilerSubCommand::Attach(opts) => {
            let target = gather_targets(&opts)?;
            let config = profiler::SamplingConfig {
                period: Some(opts.sample_period_us * 1000),
                timebase: Some(profiler::Counter::PlatformIndependent(
                    profiler::CounterId::Nanoseconds,
                )),
                sample: Some(profiler::Sample {
                    callgraph: Some(profiler::CallgraphConfig {
                        strategy: Some(profiler::CallgraphStrategy::FramePointer),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            };
            let session_opts = SessionOpts {
                symbolize: opts.symbolize,
                buffer_size_mb: opts.buffer_size_mb,
                print_stats: opts.print_stats,
                output: opts.output,
                duration: opts.duration,
                pprof_conversion: opts.pprof_conversion,
                color_output: opts.color_output,
            };
            (target, config, session_opts)
        }
        ProfilerSubCommand::Launch(opts) => {
            let component_config = if opts.test {
                profiler::AttachConfig::LaunchTest(profiler::LaunchTest {
                    url: Some(opts.url.clone()),
                    options: Some(test_manager::RunSuiteOptions {
                        test_case_filters: Some(opts.test_filters),
                        ..Default::default()
                    }),
                    ..Default::default()
                })
            } else {
                profiler::AttachConfig::LaunchComponent(profiler::LaunchComponent {
                    url: Some(opts.url.clone()),
                    moniker: opts.moniker.clone(),
                    ..Default::default()
                })
            };
            let target = profiler::TargetConfig::Component(component_config);
            let config = profiler::SamplingConfig {
                period: Some(opts.sample_period_us * 1000),
                timebase: Some(profiler::Counter::PlatformIndependent(
                    profiler::CounterId::Nanoseconds,
                )),
                sample: Some(profiler::Sample {
                    callgraph: Some(profiler::CallgraphConfig {
                        strategy: Some(profiler::CallgraphStrategy::FramePointer),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            };
            let session_opts = SessionOpts {
                symbolize: opts.symbolize,
                buffer_size_mb: opts.buffer_size_mb,
                print_stats: opts.print_stats,
                output: opts.output,
                duration: opts.duration,
                pprof_conversion: opts.pprof_conversion,
                color_output: opts.color_output,
            };
            (target, config, session_opts)
        }
        ProfilerSubCommand::Symbolize(opts) => {
            if let Ok(symbolized_record) = ffx_profiler::symbolize::symbolize(
                &opts.input,
                &opts.output.clone().into(),
                opts.pprof_conversion,
            ) {
                return ffx_profiler::pprof::samples_to_pprof(
                    symbolized_record,
                    opts.output.into(),
                );
            } else {
                anyhow::bail!("Failed to symbolize profile");
            }
        }
    };
    let config = profiler::Config {
        configs: Some(vec![config]),
        target: Some(targets),
        ..Default::default()
    };
    run_session(controller, writer, config, session_opts).await
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_gather_targets() {
        let args = args::Attach {
            pids: vec![1, 2, 3],
            tids: vec![4, 5, 6],
            job_ids: vec![7, 8, 9],
            url: None,
            buffer_size_mb: Some(8 as u64),
            moniker: None,
            duration: None,
            output: String::from("output_file"),
            ..Default::default()
        };
        let target = gather_targets(&args);
        match target {
            Ok(fidl_fuchsia_cpu_profiler::TargetConfig::Tasks(vec)) => assert!(vec.len() == 9),
            _ => assert!(false),
        }

        let empty_args = args::Attach {
            pids: vec![],
            tids: vec![],
            job_ids: vec![],
            moniker: None,
            url: None,
            buffer_size_mb: None,
            duration: None,
            output: String::from("output_file"),
            ..Default::default()
        };

        let empty_targets = gather_targets(&empty_args);
        assert!(empty_targets.is_err());

        let invalid_args1 = args::Attach {
            pids: vec![1],
            tids: vec![],
            job_ids: vec![],
            moniker: Some(String::from("core/test")),
            buffer_size_mb: Some(8 as u64),
            url: None,
            duration: None,
            output: String::from("output_file"),
            ..Default::default()
        };
        let invalid_args2 = args::Attach {
            pids: vec![],
            tids: vec![1],
            job_ids: vec![],
            moniker: Some(String::from("core/test")),
            url: None,
            buffer_size_mb: Some(8 as u64),
            duration: None,
            output: String::from("output_file"),
            ..Default::default()
        };
        let invalid_args3 = args::Attach {
            pids: vec![],
            tids: vec![],
            job_ids: vec![1],
            moniker: Some(String::from("core/test")),
            buffer_size_mb: Some(8 as u64),
            url: None,
            duration: None,
            output: String::from("output_file"),
            ..Default::default()
        };

        let invalid_targets1 = gather_targets(&invalid_args1);
        assert!(invalid_targets1.is_err());
        let invalid_targets2 = gather_targets(&invalid_args2);
        assert!(invalid_targets2.is_err());
        let invalid_targets3 = gather_targets(&invalid_args3);
        assert!(invalid_targets3.is_err());
    }
}
