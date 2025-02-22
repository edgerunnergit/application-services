// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use crate::{
    sources::ManifestSource,
    value_utils::{
        prepare_experiment, prepare_rollout, try_find_branches, try_find_features, CliUtils,
    },
    AppCommand, AppOpenArgs, ExperimentListSource, ExperimentSource, LaunchableApp, NimbusApp,
};
use anyhow::{bail, Result};
use console::Term;
use nimbus_fml::intermediate_representation::FeatureManifest;
use serde_json::{json, Value};
use std::{path::PathBuf, process::Command};

pub(crate) fn process_cmd(cmd: &AppCommand) -> Result<bool> {
    let status = match cmd {
        AppCommand::ApplyFile {
            app,
            list,
            preserve_nimbus_db,
        } => app.apply_list(list, preserve_nimbus_db)?,
        AppCommand::CaptureLogs { app, file } => app.capture_logs(file)?,
        AppCommand::Defaults {
            manifest,
            feature_id,
            output,
        } => manifest.print_defaults(feature_id.as_ref(), output.as_ref())?,
        AppCommand::Enroll {
            app,
            params,
            experiment,
            rollouts,
            branch,
            preserve_targeting,
            preserve_bucketing,
            preserve_nimbus_db,
            open,
            ..
        } => app.enroll(
            params,
            experiment,
            rollouts,
            branch,
            preserve_targeting,
            preserve_bucketing,
            preserve_nimbus_db,
            open,
        )?,
        AppCommand::ExtractFeatures {
            experiment,
            branch,
            manifest,
            feature_id,
            validate,
            multi,
            output,
        } => experiment.print_features(
            branch,
            manifest,
            feature_id.as_ref(),
            *validate,
            *multi,
            output.as_ref(),
        )?,

        AppCommand::FetchList { params, list, file } => params.fetch_list(list, file.as_ref())?,
        AppCommand::FetchRecipes {
            params,
            recipes,
            file,
        } => params.fetch_recipes(recipes, file.as_ref())?,
        AppCommand::Info { experiment, output } => experiment.print_info(output.as_ref())?,
        AppCommand::Kill { app } => app.kill_app()?,
        AppCommand::List { params, list } => list.print_list(params)?,
        AppCommand::LogState { app } => app.log_state()?,
        AppCommand::NoOp => true,
        AppCommand::Open {
            app, open: args, ..
        } => app.open(args)?,
        AppCommand::Reset { app } => app.reset_app()?,
        AppCommand::TailLogs { app } => app.tail_logs()?,
        AppCommand::Unenroll { app } => app.unenroll_all()?,
        AppCommand::ValidateExperiment {
            params,
            manifest,
            experiment,
        } => params.validate_experiment(manifest, experiment)?,
    };

    Ok(status)
}

fn prompt(term: &Term, command: &str) -> Result<()> {
    let prompt = term.style().cyan();
    let style = term.style().yellow();
    term.write_line(&format!(
        "{} {}",
        prompt.apply_to("$"),
        style.apply_to(command)
    ))?;
    Ok(())
}

fn output_ok(term: &Term, title: &str) -> Result<()> {
    let style = term.style().green();
    term.write_line(&format!("✅ {}", style.apply_to(title)))?;
    Ok(())
}

fn output_err(term: &Term, title: &str, detail: &str) -> Result<()> {
    let style = term.style().red();
    term.write_line(&format!("❎ {}: {detail}", style.apply_to(title),))?;
    Ok(())
}

impl LaunchableApp {
    fn exe(&self) -> Result<Command> {
        Ok(match self {
            Self::Android { device_id, .. } => {
                let adb_name = if std::env::consts::OS != "windows" {
                    "adb"
                } else {
                    "adb.exe"
                };
                let adb = std::env::var("ADB_PATH").unwrap_or_else(|_| adb_name.to_string());
                let mut cmd = Command::new(adb);
                if let Some(id) = device_id {
                    cmd.args(["-s", id]);
                }
                cmd
            }
            Self::Ios { .. } => {
                if std::env::consts::OS != "macos" {
                    panic!("Cannot run commands for iOS on anything except macOS");
                }
                let xcrun = std::env::var("XCRUN_PATH").unwrap_or_else(|_| "xcrun".to_string());
                let mut cmd = Command::new(xcrun);
                cmd.arg("simctl");
                cmd
            }
        })
    }

    fn kill_app(&self) -> Result<bool> {
        Ok(match self {
            Self::Android { package_name, .. } => self
                .exe()?
                .arg("shell")
                .arg(format!("am force-stop {}", package_name))
                .spawn()?
                .wait()?
                .success(),
            Self::Ios {
                app_id, device_id, ..
            } => {
                let _ = self
                    .exe()?
                    .args(["terminate", device_id, app_id])
                    .output()?;
                true
            }
        })
    }

    fn unenroll_all(&self) -> Result<bool> {
        let payload = json! {{ "data": [] }};
        self.start_app(false, Some(&payload), true, &Default::default())
    }

    fn reset_app(&self) -> Result<bool> {
        Ok(match self {
            Self::Android { package_name, .. } => self
                .exe()?
                .arg("shell")
                .arg(format!("pm clear {}", package_name))
                .spawn()?
                .wait()?
                .success(),
            Self::Ios {
                app_id, device_id, ..
            } => {
                self.exe()?
                    .args(["privacy", device_id, "reset", "all", app_id])
                    .status()?;
                let data = self.ios_app_container("data")?;
                let groups = self.ios_app_container("groups")?;
                self.ios_reset(data, groups)?;
                true
            }
        })
    }

    fn tail_logs(&self) -> Result<bool> {
        let term = Term::stdout();
        let _ = term.clear_screen();
        Ok(match self {
            Self::Android { .. } => {
                let mut args = logcat_args();
                args.append(&mut vec!["-v", "color"]);
                prompt(&term, &format!("adb {}", args.join(" ")))?;
                self.exe()?.args(args).spawn()?.wait()?.success()
            }
            Self::Ios { .. } => {
                prompt(
                    &term,
                    &format!("{} | xargs tail -f", self.ios_log_file_command()),
                )?;
                let log = self.ios_log_file()?;

                Command::new("tail")
                    .arg("-f")
                    .arg(log.as_path().to_str().unwrap())
                    .spawn()?
                    .wait()?
                    .success()
            }
        })
    }

    fn capture_logs(&self, file: &PathBuf) -> Result<bool> {
        let term = Term::stdout();
        Ok(match self {
            Self::Android { .. } => {
                let mut args = logcat_args();
                args.append(&mut vec!["-d"]);
                prompt(
                    &term,
                    &format!(
                        "adb {} > {}",
                        args.join(" "),
                        file.as_path().to_str().unwrap()
                    ),
                )?;
                let output = self.exe()?.args(args).output()?;
                std::fs::write(file, String::from_utf8_lossy(&output.stdout).to_string())?;
                true
            }

            Self::Ios { .. } => {
                let log = self.ios_log_file()?;
                prompt(
                    &term,
                    &format!(
                        "{} | xargs -J %log_file% cp %log_file% {}",
                        self.ios_log_file_command(),
                        file.as_path().to_str().unwrap()
                    ),
                )?;
                std::fs::copy(log, file)?;
                true
            }
        })
    }

    fn ios_log_file(&self) -> Result<PathBuf> {
        let data = self.ios_app_container("data")?;
        let mut files = glob::glob(&format!("{}/**/*.log", data))?;
        let log = files.next();
        Ok(log.ok_or_else(|| {
            anyhow::Error::msg(
                "Logs are not available before the app is started for the first time",
            )
        })??)
    }

    fn ios_log_file_command(&self) -> String {
        if let Self::Ios {
            device_id, app_id, ..
        } = self
        {
            format!(
                "find $(xcrun simctl get_app_container {0} {1} data) -name \\*.log",
                device_id, app_id
            )
        } else {
            unreachable!()
        }
    }

    fn log_state(&self) -> Result<bool> {
        self.start_app(false, None, true, &Default::default())
    }

    #[allow(clippy::too_many_arguments)]
    fn enroll(
        &self,
        params: &NimbusApp,
        experiment: &ExperimentSource,
        rollouts: &Vec<ExperimentSource>,
        branch: &str,
        preserve_targeting: &bool,
        preserve_bucketing: &bool,
        preserve_nimbus_db: &bool,
        open: &AppOpenArgs,
    ) -> Result<bool> {
        let term = Term::stdout();

        let experiment = Value::try_from(experiment)?;
        let slug = experiment.get_str("slug")?.to_string();

        let mut recipes = vec![prepare_experiment(
            &experiment,
            params,
            branch,
            *preserve_targeting,
            *preserve_bucketing,
        )?];
        prompt(
            &term,
            &format!("# Enrolling in the '{0}' branch of '{1}'", branch, &slug),
        )?;

        for r in rollouts {
            let rollout = Value::try_from(r)?;
            let slug = rollout.get_str("slug")?.to_string();
            recipes.push(prepare_rollout(
                &rollout,
                params,
                *preserve_targeting,
                *preserve_bucketing,
            )?);
            prompt(&term, &format!("# Enrolling into the '{0}' rollout", &slug))?;
        }

        let payload = json! {{ "data": recipes }};
        self.start_app(!preserve_nimbus_db, Some(&payload), true, open)
    }

    fn apply_list(&self, list: &ExperimentListSource, preserve_nimbus_db: &bool) -> Result<bool> {
        let value: Value = list.try_into()?;

        self.start_app(!preserve_nimbus_db, Some(&value), true, &Default::default())
    }

    fn ios_app_container(&self, container: &str) -> Result<String> {
        if let Self::Ios {
            app_id, device_id, ..
        } = self
        {
            // We need to get the app container directories, and delete them.
            let output = self
                .exe()?
                .args(["get_app_container", device_id, app_id, container])
                .output()
                .expect("Expected an app-container from the simulator");
            let string = String::from_utf8_lossy(&output.stdout).to_string();
            Ok(string.trim().to_string())
        } else {
            unreachable!()
        }
    }

    fn ios_reset(&self, data_dir: String, groups_string: String) -> Result<bool> {
        let term = Term::stdout();
        prompt(&term, "# Resetting the app")?;
        if !data_dir.is_empty() {
            prompt(&term, &format!("rm -Rf {}/* 2>/dev/null", data_dir))?;
            let _ = std::fs::remove_dir_all(&data_dir);
            let _ = std::fs::create_dir_all(&data_dir);
        }
        let lines = groups_string.split('\n');

        for line in lines {
            let words = line.splitn(2, '\t').collect::<Vec<_>>();
            if let [_, dir] = words.as_slice() {
                if !dir.is_empty() {
                    prompt(&term, &format!("rm -Rf {}/* 2>/dev/null", dir))?;
                    let _ = std::fs::remove_dir_all(dir);
                    let _ = std::fs::create_dir_all(dir);
                }
            }
        }
        Ok(true)
    }

    fn open(&self, open: &AppOpenArgs) -> Result<bool> {
        self.start_app(false, None, false, open)
    }

    fn create_deeplink(&self, open: &AppOpenArgs) -> Result<Option<String>> {
        let deeplink = &open.deeplink;
        if deeplink.is_none() {
            return Ok(None);
        }
        let deeplink = deeplink.as_ref().unwrap();
        Ok(if deeplink.contains("://") {
            Some(deeplink.clone())
        } else if let Some(scheme) = match self {
            Self::Android { scheme, .. } | Self::Ios { scheme, .. } => scheme,
        } {
            Some(format!("{}://{}", scheme, deeplink))
        } else {
            anyhow::bail!("Cannot use a deeplink without a scheme for this app")
        })
    }

    fn start_app(
        &self,
        reset_db: bool,
        payload: Option<&Value>,
        log_state: bool,
        open: &AppOpenArgs,
    ) -> Result<bool> {
        Ok(match self {
            Self::Android { .. } => self
                .android_start(reset_db, payload, log_state, open)?
                .spawn()?
                .wait()?
                .success(),
            Self::Ios { .. } => self
                .ios_start(reset_db, payload, log_state, open)?
                .spawn()?
                .wait()?
                .success(),
        })
    }

    fn android_start(
        &self,
        reset_db: bool,
        json: Option<&Value>,
        log_state: bool,
        open: &AppOpenArgs,
    ) -> Result<Command> {
        if let Self::Android {
            package_name,
            activity_name,
            ..
        } = self
        {
            let mut args: Vec<String> = Vec::new();

            let (start_args, ending_args) = open.args();
            args.extend_from_slice(start_args);

            if let Some(deeplink) = self.create_deeplink(open)? {
                args.extend([
                    "-a android.intent.action.VIEW".to_string(),
                    "-c android.intent.category.DEFAULT".to_string(),
                    "-c android.intent.category.BROWSABLE".to_string(),
                    format!("-d {}", deeplink),
                ]);
            } else {
                args.extend([
                    format!("-n {}/{}", package_name, activity_name),
                    "-a android.intent.action.MAIN".to_string(),
                    "-c android.intent.category.LAUNCHER".to_string(),
                ]);
            }

            if log_state || json.is_some() || reset_db {
                args.extend(["--esn nimbus-cli".to_string(), "--ei version 1".to_string()]);
            }

            if reset_db {
                args.push("--ez reset-db true".to_string());
            }
            if let Some(s) = json {
                let json = s.to_string().replace('\'', "&apos;");
                args.push(format!("--es experiments '{}'", json))
            }
            if log_state {
                args.push("--ez log-state true".to_string());
            };
            args.extend_from_slice(ending_args);

            let mut cmd = self.exe()?;
            let sh = format!(r#"am start {}"#, args.join(" \\\n        "),);
            cmd.arg("shell").arg(&sh);
            let term = Term::stdout();
            prompt(&term, &format!("adb shell \"{}\"", sh))?;
            Ok(cmd)
        } else {
            unreachable!();
        }
    }

    fn ios_start(
        &self,
        reset_db: bool,
        json: Option<&Value>,
        log_state: bool,
        open: &AppOpenArgs,
    ) -> Result<Command> {
        if let Self::Ios {
            app_id, device_id, ..
        } = self
        {
            let mut args: Vec<String> = Vec::new();

            let (starting_args, ending_args) = open.args();

            let mut is_launch = false;
            if let Some(deeplink) = self.create_deeplink(open)? {
                args.push("openurl".to_string());
                args.extend_from_slice(starting_args);
                args.extend([device_id.to_string(), deeplink]);
            } else {
                args.push("launch".to_string());
                args.extend_from_slice(starting_args);
                args.extend([device_id.to_string(), app_id.to_string()]);
                is_launch = true;
            }

            // Doing this here because we may be able to change the mechanism of passing
            // arguments to the iOS apps at a later stage.
            let disallowed_by_openurl = |msg: &str| -> Result<()> {
                if !is_launch {
                    bail!(format!("The iOS simulator's openurl command doesn't support command line arguments which {} relies upon", msg));
                } else {
                    Ok(())
                }
            };

            if log_state || json.is_some() || reset_db {
                args.extend([
                    "--nimbus-cli".to_string(),
                    "--version".to_string(),
                    "1".to_string(),
                ]);
            }

            if reset_db {
                // We don't check launch here, because reset-db is never used
                // without enroll.
                args.push("--reset-db".to_string());
            }
            if let Some(s) = json {
                disallowed_by_openurl("enroll and test-feature")?;
                args.extend([
                    "--experiments".to_string(),
                    s.to_string().replace('\'', "&apos;"),
                ]);
            }
            if log_state {
                disallowed_by_openurl("log-state")?;
                args.push("--log-state".to_string());
            }
            args.extend_from_slice(ending_args);

            let mut cmd = self.exe()?;
            cmd.args(args.clone());

            let sh = format!(r#"xcrun simctl {}"#, args.join(" \\\n        "),);
            let term = Term::stdout();
            prompt(&term, &sh)?;
            Ok(cmd)
        } else {
            unreachable!()
        }
    }
}

fn logcat_args<'a>() -> Vec<&'a str> {
    vec!["logcat", "-b", "main"]
}

impl NimbusApp {
    fn validate_experiment(
        &self,
        manifest_source: &ManifestSource,
        experiment: &ExperimentSource,
    ) -> Result<bool> {
        let term = Term::stdout();
        let value: Value = experiment.try_into()?;

        let manifest = match TryInto::<FeatureManifest>::try_into(manifest_source) {
            Ok(manifest) => {
                output_ok(&term, &format!("Loaded manifest from {manifest_source}"))?;
                manifest
            }
            Err(err) => {
                output_err(
                    &term,
                    &format!("Problem with manifest from {manifest_source}"),
                    &err.to_string(),
                )?;
                bail!("Error when loading and validating the manifest");
            }
        };

        let mut is_valid = true;
        for b in try_find_branches(&value)? {
            let branch = b.get_str("slug")?;
            for f in try_find_features(&b)? {
                let id = f.get_str("featureId")?;
                let value = f
                    .get("value")
                    .unwrap_or_else(|| panic!("Branch {branch} feature {id} has no value"));
                let res = manifest.validate_feature_config(id, value.clone());
                match res {
                    Ok(_) => output_ok(&term, &format!("{branch: <15} {id}"))?,
                    Err(err) => {
                        is_valid = false;
                        output_err(&term, &format!("{branch: <15} {id}"), &err.to_string())?
                    }
                }
            }
        }
        if !is_valid {
            bail!("At least one error detected");
        }
        Ok(true)
    }
}
