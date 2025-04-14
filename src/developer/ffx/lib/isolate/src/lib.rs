// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::{anyhow, Context as _, Result};
use ffx_config::{global_env_context, EnvironmentContext, SdkRoot};
use ffx_executor::{CommandOutput, FfxExecutor};
use sdk::FfxSdkConfig;
use serde::Serialize;
use serde_json::Value;
use std::borrow::Cow;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Child;
use std::time::SystemTime;
use tempfile::TempDir;

/// Where to search for ffx and subtools, based on either being part of an
/// ffx command (like `ffx self-test`) or being part of the build (using the
/// build root to find things in either the host tool or test data targets.
#[derive(Debug, Clone)]
pub enum SearchContext {
    Runtime { ffx_path: PathBuf, sdk_root: Option<SdkRoot>, subtool_search_paths: Vec<PathBuf> },
    Build { build_root: PathBuf },
}

impl SearchContext {
    fn sdk_config(&self) -> Option<FfxSdkConfig> {
        match self {
            SearchContext::Runtime { sdk_root: Some(sdk_root), .. } => Some(sdk_root.to_config()),
            SearchContext::Build { build_root } => {
                // TODO(392136182): Do not refer to hardcoded SDK paths. This support is being
                // removed.
                let root = Some(build_root.join("sdk/exported/core"));
                Some(FfxSdkConfig { root, manifest: None, module: None })
            }
            _ => None,
        }
    }
}

pub(crate) fn env_search_paths(search: &SearchContext) -> Vec<Cow<'_, Path>> {
    use SearchContext::*;
    match search {
        Runtime { subtool_search_paths, .. } => {
            subtool_search_paths.iter().map(|p| Cow::Borrowed(p.as_ref())).collect()
        }
        Build { build_root } => {
            // The build passes these search paths in so that when this is run from
            // a unit test we can find the path that ffx subtools exist at from
            // the build root.
            vec![
                Cow::Owned(build_root.join(std::env!("SUBTOOL_SEARCH_TEST_DATA"))),
                Cow::Owned(build_root.join(std::env!("SUBTOOL_SEARCH_HOST_TOOLS"))),
            ]
        }
    }
}

pub(crate) fn find_ffx(search: &SearchContext, search_paths: &[Cow<'_, Path>]) -> Result<PathBuf> {
    use SearchContext::*;
    match search {
        Runtime { ffx_path, .. } => return Ok(ffx_path.to_owned()),
        Build { .. } => {
            for path in search_paths {
                let path = path.join("ffx");
                if path.exists() {
                    return Ok(path);
                }
            }
        }
    }
    Err(anyhow!(
        "ffx not found in search paths for isolation. cwd={}, search_paths={search_paths:?}",
        std::env::current_dir()?.display()
    ))
}

pub(crate) fn get_log_dir(log_dir_root: &TempDir) -> PathBuf {
    if let Ok(d) = std::env::var("FUCHSIA_TEST_OUTDIR") {
        // If this is the daemon, and we use the same dir as the parent,
        // the two daemons will race to write the same file. So instead let's
        // always try to create a subdir when in the infra environment.
        // To do so, we take the tail of the tmpdir (which Path calls
        // file_name() even when actually a directory), and add it
        // to the end of FUCHSIA_TEST_OUTDIR, to give the new subdirectory.
        // Because the tail of the tmpdir includes the "name", we'll be able
        // to associate the log directory with the isolated test.
        let mut pb = PathBuf::from(d);
        if let Some(tmptail) = log_dir_root.path().file_name() {
            pb.push(tmptail);
        }
        pb
    } else {
        log_dir_root.path().join("log")
    }
}

pub(crate) fn create_directories(log_dir: &PathBuf, log_dir_root: &TempDir) -> Result<()> {
    std::fs::create_dir_all(&log_dir)?;
    let metrics_path = log_dir_root.path().join("metrics_home/.fuchsia/metrics");
    std::fs::create_dir_all(&metrics_path)?;

    // TODO(287694118): See if we should get isolate-dir itself to deal with metrics isolation.

    // Mark that analytics are disabled
    std::fs::write(metrics_path.join("analytics-status"), "0")?;
    // Mark that the notice has been given
    std::fs::write(metrics_path.join("ffx"), "1")?;
    Ok(())
}

/// Isolate provides an abstraction around an isolated configuration environment for `ffx`.
pub struct Isolate {
    tmpdir: TempDir,
    log_dir: PathBuf,
    env_ctx: ffx_config::EnvironmentContext,
}

impl FfxExecutor for Isolate {
    fn make_ffx_cmd(&self, args: &[&str]) -> Result<std::process::Command> {
        let mut cmd = self.env_context().rerun_prefix()?;
        cmd.args(args);
        Ok(cmd)
    }
}

impl Isolate {
    /// Creates a new isolated environment for ffx to run in, including a
    /// user level configuration that isolates the ascendd socket into a temporary
    /// directory. If $FUCHSIA_TEST_OUTDIR is set, then it is used to specify the log
    /// directory. The isolated environment is torn down when the Isolate is
    /// dropped, which will attempt to terminate any running daemon and then
    /// remove all isolate files.
    ///
    /// Most of the time you'll want to use the appropriate convenience wrapper,
    /// [`Isolate::new_with_sdk`] or [`Isolate::new_in_test`].
    #[allow(clippy::unused_async)] // TODO(https://fxbug.dev/386387845)
    pub async fn new_with_search(
        name: &str,
        search: SearchContext,
        ssh_key: PathBuf,
        env_context: &EnvironmentContext,
    ) -> Result<Isolate> {
        let tmpdir = tempfile::Builder::new().prefix(name).tempdir()?;
        let search_paths = env_search_paths(&search);

        let ffx_path = find_ffx(&search, &search_paths)?;

        let sdk_config = search.sdk_config();
        let log_dir = get_log_dir(&tmpdir);
        create_directories(&log_dir, &tmpdir)?;
        let mut mdns_discovery = true;
        let mut target_addr = None;
        if let Some(addr) =
            std::env::var("FUCHSIA_DEVICE_ADDR").ok().filter(|addr| !addr.is_empty())
        {
            // When run in infra, disable mdns discovery.
            // TODO(https://fxbug.dev/42121155): Remove when we have proper network isolation.
            target_addr = Option::Some(Cow::Owned(addr + ":0"));
            mdns_discovery = false;
        }
        let log_target_levels = env_context.query("log.target_levels").get()?;
        // Propagate log configuration information to the isolate.
        // TODO(396473745): we should propagate _all_ log values,
        // except possibly log.dir (which may be set above from
        // FUCHSIA_TEST_OUTDIR)
        let log_level = env_context.query("log.level").get()?;

        let user_config = UserConfig::for_test(
            log_dir.to_string_lossy(),
            log_level,
            log_target_levels,
            target_addr,
            mdns_discovery,
            search_paths,
            sdk_config,
        );
        std::fs::write(
            tmpdir.path().join(".ffx_user_config.json"),
            serde_json::to_string(&user_config)?,
        )?;

        std::fs::write(
            tmpdir.path().join(".ffx_env"),
            serde_json::to_string(&FfxEnvConfig::for_test(
                tmpdir.path().join(".ffx_user_config.json").to_string_lossy(),
            ))?,
        )?;

        let mut env_vars = HashMap::new();

        // Pass along all temp related variables, so as to avoid anything
        // falling back to writing into /tmp. In our CI environment /tmp is
        // extremely limited, whereas invocations of tests are provided
        // dedicated temporary areas.
        // We should propagate PATH to children, because it may contain
        // changes e.g. that point to vendored binaries.
        // Propagate SYMBOL_INDEX_INCLUDE and GCE_METADATA_HOST too, which
        // contains extra URLs and authentication info that are necessary for
        // symbolization.
        for (var, val) in std::env::vars() {
            if var.contains("TEMP")
                || var.contains("TMP")
                || var == "PATH"
                || var == "SYMBOL_INDEX_INCLUDE"
                || var == "GCE_METADATA_HOST"
            {
                let _ = env_vars.insert(var, val);
            }
        }

        let _ = env_vars.insert(
            "HOME".to_owned(),
            tmpdir.path().join("metrics_home").to_string_lossy().to_string(),
        );

        let _ = env_vars.insert(
            ffx_config::EnvironmentContext::FFX_BIN_ENV.to_owned(),
            ffx_path.to_string_lossy().to_string(),
        );

        // On developer systems, FUCHSIA_SSH_KEY is normally not set, and so ffx
        // looks up an ssh key via a $HOME heuristic, however that is broken by
        // isolation. ffx also however respects the FUCHSIA_SSH_KEY environment
        // variable natively, so, fetching the ssh key path from the config, and
        // then passing that expanded path along explicitly is sufficient to
        // ensure that the isolate has a viable key path.
        let _ =
            env_vars.insert("FUCHSIA_SSH_KEY".to_owned(), ssh_key.to_string_lossy().to_string());

        let env_ctx = ffx_config::EnvironmentContext::isolated(
            env_context.exe_kind(),
            tmpdir.path().to_owned(),
            env_vars,
            ffx_config::ConfigMap::new(),
            Some(tmpdir.path().join(".ffx_env")),
            None,
            env_context.has_no_environment(),
        )?;

        // NOTE: config values from this Isolate might not be found correctly,
        // due to issues with caching.  Until this is fixed (TODO(https://fxbug.dev/42075364)),
        // callers should call `ffx_config::cache_invalidate()` if they will be
        // querying config values, e.g. "log.dir".
        Ok(Isolate { tmpdir, log_dir, env_ctx })
    }

    /// Simple wrapper around [`Isolate::new_with_search`] for situations where all you
    /// have is the path to ffx. You should prefer to use [`Isolate::new_with_sdk`] or
    /// [`Isolate::new_in_test`] if you can.
    pub async fn new(name: &str, ffx_path: PathBuf, ssh_key: PathBuf) -> Result<Self> {
        // assume subtools are in the same directory as the ffx that ran this
        let subtool_search_paths = ffx_path.parent().map_or_else(|| vec![], |p| vec![p.to_owned()]);
        let search = SearchContext::Runtime { ffx_path, sdk_root: None, subtool_search_paths };
        let env_context = global_env_context().context("No global context")?;
        Self::new_with_search(name, search, ssh_key, &env_context).await
    }

    /// Use this when building an isolation environment from within an ffx subtool
    /// or other situation where there's an sdk involved.
    pub async fn new_with_sdk(
        name: &str,
        ssh_key: PathBuf,
        context: &EnvironmentContext,
    ) -> Result<Self> {
        let ffx_path = context.rerun_bin()?;
        let ffx_path =
            std::fs::canonicalize(ffx_path).context("could not canonicalize own path")?;

        let sdk_root = context.get_sdk_root().ok();
        let subtool_search_paths =
            context.query("ffx.subtool-search-paths").get().unwrap_or_default();

        Self::new_with_search(
            name,
            SearchContext::Runtime { ffx_path, sdk_root, subtool_search_paths },
            ssh_key,
            context,
        )
        .await
    }

    /// Use this when building an isolation environment from within a unit test
    /// in the fuchsia tree. This will make the isolated ffx look for subtools
    /// in the appropriate places in the build tree.
    ///
    /// Note: This function assumes the test is being run from the build root.
    /// If not, you can use [`Self::new_with_search`] to make it explicit.
    pub async fn new_in_test(
        name: &str,
        ssh_key: PathBuf,
        context: &EnvironmentContext,
    ) -> Result<Self> {
        let build_root = std::env::current_dir()?;
        Self::new_with_search(name, SearchContext::Build { build_root }, ssh_key, context).await
    }

    pub fn log_dir(&self) -> &Path {
        &self.log_dir
    }

    pub fn dir(&self) -> &Path {
        self.tmpdir.path()
    }

    pub fn ascendd_path(&self) -> PathBuf {
        self.tmpdir.path().join("daemon.sock")
    }

    pub fn env_context(&self) -> &EnvironmentContext {
        &self.env_ctx
    }

    // Manually spawning the daemon allow it to remain under our process group instead of
    // daemonizing. These daemons will be sent signals directed towards this process group.
    pub async fn start_daemon(&self) -> Result<Child> {
        let daemon = ffx_daemon::run_daemon(self.env_context()).await?;
        const DAEMON_WAIT_TIME: u64 = 2000;
        // Wait a bit to make sure the daemon has had a chance to start up.
        fuchsia_async::Timer::new(fuchsia_async::MonotonicDuration::from_millis(DAEMON_WAIT_TIME))
            .await;
        Ok(daemon)
    }

    // TODO(396006570): Remove these functions once migrations have been done in external
    // users.
    pub async fn ffx_cmd(&self, args: &[&str]) -> Result<std::process::Command> {
        std::future::ready(FfxExecutor::make_ffx_cmd(self, args)).await
    }

    pub async fn ffx(&self, args: &[&str]) -> Result<CommandOutput> {
        FfxExecutor::exec_ffx(self, args).await.map_err(Into::into)
    }
}

#[derive(Serialize, Debug)]
struct UserConfig<'a> {
    daemon: UserConfigDaemon,
    log: UserConfigLog<'a>,
    test: UserConfigTest,
    targets: UserConfigTargets<'a>,
    discovery: UserConfigDiscovery,
    ffx: UserConfigFfx<'a>,
    sdk: Option<FfxSdkConfig>,
}

#[derive(Serialize, Debug)]
struct UserConfigLog<'a> {
    enabled: bool,
    level: Option<String>,
    // For target_levels, we'd like to use a HashMap<String, String> or even
    // a serde_json::Map<String, String>, but ffx_config doesn't returning
    // support maps -- TODO(https://fxbug.dev/42075137). So for now we're stuck with getting a
    // serde_json::Value directly, and hoping it's the right type.  At least
    // we're no worse off than our caller is, since if target_levels isn't a
    // String=>String map, then nothing using this config entry was going to
    // work anyway.
    target_levels: Option<Value>,
    dir: Cow<'a, str>,
}

#[derive(Serialize, Debug)]
struct UserConfigFfx<'a> {
    #[serde(rename = "subtool-search-paths")]
    subtool_search_paths: Vec<Cow<'a, Path>>,
}

#[derive(Serialize, Debug)]
struct UserConfigTest {
    #[serde(rename(serialize = "is-isolated"))]
    is_isolated: bool,
}

#[derive(Serialize, Debug)]
struct UserConfigTargets<'a> {
    manual: HashMap<Cow<'a, str>, Option<SystemTime>>,
}

#[derive(Serialize, Debug)]
struct UserConfigDiscovery {
    mdns: UserConfigMdns,
}

#[derive(Serialize, Debug)]
struct UserConfigMdns {
    enabled: bool,
}

#[derive(Serialize, Debug)]
struct UserConfigDaemon {
    autostart: bool,
}

impl<'a> UserConfig<'a> {
    fn for_test(
        log_dir: Cow<'a, str>,
        log_level: Option<String>,
        log_target_levels: Option<Value>,
        target: Option<Cow<'a, str>>,
        discovery: bool,
        subtool_search_paths: Vec<Cow<'a, Path>>,
        sdk: Option<FfxSdkConfig>,
    ) -> Self {
        let mut manual_targets = HashMap::new();
        if !target.is_none() {
            manual_targets.insert(target.unwrap(), None);
        }
        Self {
            log: UserConfigLog {
                enabled: true,
                level: log_level,
                target_levels: log_target_levels,
                dir: log_dir,
            },
            test: UserConfigTest { is_isolated: true },
            targets: UserConfigTargets { manual: manual_targets },
            discovery: UserConfigDiscovery { mdns: UserConfigMdns { enabled: discovery } },
            ffx: UserConfigFfx { subtool_search_paths },
            sdk,
            daemon: UserConfigDaemon { autostart: false },
        }
    }
}

#[derive(Serialize, Debug)]
struct FfxEnvConfig<'a> {
    user: Cow<'a, str>,
    build: Option<&'static str>,
    global: Option<&'static str>,
}

impl<'a> FfxEnvConfig<'a> {
    fn for_test(user: Cow<'a, str>) -> Self {
        Self { user, build: None, global: None }
    }
}
