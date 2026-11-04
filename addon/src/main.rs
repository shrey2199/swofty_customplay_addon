use std::fs::{self, File};
use std::io::{self, BufRead, Write};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use serde_json::{Value, json};

mod updates;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

struct Host<R, W> {
    input: R,
    output: W,
    next_id: u64,
}

impl<R: BufRead, W: Write> Host<R, W> {
    fn read(&mut self) -> Result<Option<Value>> {
        let mut line = String::new();
        if self.input.read_line(&mut line)? == 0 {
            return Ok(None);
        }
        Ok(Some(serde_json::from_str(&line)?))
    }

    fn send(&mut self, value: Value) -> Result<()> {
        writeln!(self.output, "{value}")?;
        self.output.flush()?;
        Ok(())
    }

    fn call(&mut self, method: &str) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        self.send(json!({"id": id, "method": method, "params": null}))?;
        let response = self.read()?.ok_or("The launcher closed the connection")?;
        if response["id"] != id {
            return Err("Unexpected reply from the launcher".into());
        }
        if let Some(error) = response.get("error") {
            return Err(error.to_string().into());
        }
        Ok(response["result"].clone())
    }
}

#[derive(Default)]
struct Addon {
    settings_dir: PathBuf,
    launcher_data: PathBuf,
    profile: String,
    child: Option<Child>,
    status: Option<(String, &'static str)>,
    updates: updates::Updates,
}

impl Addon {
    fn settings_path(&self) -> PathBuf {
        // Keep the original settings file so existing batch selections carry over.
        self.settings_dir.join("batches.json")
    }

    fn loaders(&self) -> Result<std::collections::BTreeMap<PathBuf, PathBuf>> {
        match fs::read(self.settings_path()) {
            Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Default::default()),
            Err(e) => Err(e.into()),
        }
    }

    fn log_path(&self) -> PathBuf {
        self.settings_dir.join("launch.log")
    }

    fn game<R: BufRead, W: Write>(&self, host: &mut Host<R, W>) -> Result<Option<PathBuf>> {
        Ok(host.call("game_dir")?.as_str().map(PathBuf::from))
    }

    fn observe_child(&mut self) -> Result<bool> {
        let Some(child) = self.child.as_mut() else {
            return Ok(false);
        };
        let Some(exit) = child.try_wait()? else {
            return Ok(false);
        };
        self.child = None;
        self.status = Some(if exit.success() {
            ("Custom session finished.".into(), "good")
        } else {
            (
                format!("Custom launch ended with {exit}. Open the launch log for details."),
                "bad",
            )
        });
        Ok(true)
    }

    fn view<R: BufRead, W: Write>(&mut self, host: &mut Host<R, W>) -> Result<Value> {
        self.observe_child()?;
        self.updates.poll();
        let game = self.game(host)?;
        let loaders = self.loaders()?;
        let loader = game.as_ref().and_then(|g| loaders.get(g));
        let mut actions = Vec::new();
        if game.is_some() && self.child.is_none() {
            actions.push(json!({"id":"choose", "label":"Choose loader file…", "kind": {
                "pick": {"title":"Choose a .bat or .exe loader file", "extensions":["bat","exe"], "folders":false, "multiple":false}
            }}));
            if loader.is_some_and(|path| validate_loader(path).is_ok()) {
                actions.push(json!({"id":"play", "label":"Play Custom", "style":"primary", "icon":"play"}));
            }
        }
        if self.log_path().is_file() {
            actions.push(json!({"id":"log", "label":"Open launch log"}));
        }
        let mut notices = self
            .status
            .iter()
            .map(|(text, tone)| json!({"text":text,"tone":tone}))
            .collect::<Vec<_>>();
        let mut blocks = vec![json!({"facts": [
            {"label":"Profile", "value":self.profile},
            {"label":"Game folder", "value":game.map(|g|g.display().to_string()).unwrap_or_else(|| "Select a game folder in the launcher".into())},
            {"label":"Loader file", "value":loader.map(|path|path.display().to_string()).unwrap_or_else(|| "Choose a .bat or .exe file".into())}
        ]})];
        self.updates
            .view(self.child.is_some(), &mut actions, &mut notices, &mut blocks);
        Ok(json!({
            "subtitle": "Launch the active profile through a .bat or .exe loader file.",
            "actions": actions,
            "notices": notices,
            "tick": true,
            "body": {"blocks": blocks}
        }))
    }

    fn start<R: BufRead, W: Write>(&mut self, host: &mut Host<R, W>, game: &Path) -> Result<Value> {
        self.observe_child()?;
        if self.child.is_some() || host.call("game_status")?["running"] == true {
            return Err(
                "Close the game and wait for the current session to finish before launching again".into(),
            );
        }
        let loader = self
            .loaders()?
            .remove(game)
            .ok_or("Choose a loader file for this game first")?;
        validate_loader(&loader)?;
        let exe = std::env::current_exe()?;
        let bin = exe.parent().ok_or("The addon has no bin folder")?;
        let root = bin.parent().ok_or("The addon has no installation folder")?;
        let runner = bin.join("swofty.exe");
        let runtime = root.join("loader");
        if !runner.is_file() || !runtime.join("version.dll").is_file() {
            return Err("The bundled runner or loader is missing. Reinstall Custom Play.".into());
        }
        fs::create_dir_all(&self.settings_dir)?;
        let log = File::create(self.log_path())?;
        self.child = Some(
            Command::new(runner)
                .arg("--data-dir")
                .arg(&self.launcher_data)
                .arg("--profile")
                .arg(&self.profile)
                .args(["run", "--apply", "--game"])
                .arg(game)
                .env("SWOFTY_CUSTOM_LOADER", loader)
                .env("SWOFTY_LOADER_DIR", runtime)
                .creation_flags(0x08000000) // CREATE_NO_WINDOW
                .stdin(Stdio::null())
                .stdout(log.try_clone()?)
                .stderr(log)
                .spawn()?,
        );
        self.status = Some((
            "Custom session started. See the launch log for progress.".into(),
            "accent",
        ));
        Ok(json!("reload"))
    }

    fn event<R: BufRead, W: Write>(&mut self, host: &mut Host<R, W>, event: &Value) -> Result<Value> {
        if event["click"]["id"] == "log" {
            return Ok(json!({"open": self.log_path()}));
        }
        if event["click"]["id"] == "update" {
            self.observe_child()?;
            if self.child.is_some() || host.call("game_status")?["running"] == true {
                return Err("Finish the game session before updating Custom Play".into());
            }
            self.updates.start(&self.launcher_data)?;
            return Ok(
                json!({"notice":"Preparing the Custom Play update. Swofty will restart when the download is verified."}),
            );
        }
        let game = self
            .game(host)?
            .ok_or("Select a game folder in the launcher first")?;
        if event["picked"]["id"] == "choose" {
            if let Some(path) = event["picked"]["paths"][0].as_str() {
                let loader = PathBuf::from(path);
                validate_loader(&loader)?;
                let mut loaders = self.loaders()?;
                loaders.insert(game, loader);
                fs::create_dir_all(&self.settings_dir)?;
                let temporary = self
                    .settings_dir
                    .join(format!("batches.{}.tmp", std::process::id()));
                fs::write(&temporary, serde_json::to_vec_pretty(&loaders)?)?;
                fs::rename(temporary, self.settings_path())?;
            }
            return Ok(json!("reload"));
        }
        if event["click"]["id"] == "play" {
            return self.start(host, &game);
        }
        Ok(json!("none"))
    }

    fn call<R: BufRead, W: Write>(&mut self, host: &mut Host<R, W>, request: &Value) -> Result<Value> {
        let params = &request["params"];
        match request["method"].as_str() {
            Some("hello") => {
                self.settings_dir = PathBuf::from(params["data_dir"].as_str().ok_or("No addon data folder")?);
                self.launcher_data = self
                    .settings_dir
                    .parent()
                    .filter(|p| p.file_name().is_some_and(|n| n == "addon-data"))
                    .and_then(Path::parent)
                    .ok_or("Unexpected addon data folder layout")?
                    .to_owned();
                self.profile = params["profile"]["name"]
                    .as_str()
                    .ok_or("No active profile")?
                    .into();
                self.updates.init(params["launcher"].as_str(), &self.settings_dir);
                Ok(json!({"api": 1}))
            }
            Some("profile") => {
                self.profile = params["name"].as_str().ok_or("No active profile")?.into();
                Ok(Value::Null)
            }
            Some("view") => self.view(host),
            Some("event") => self.event(host, &params["event"]),
            Some("tick") => {
                let updated = self.updates.poll();
                if self.observe_child()? || updated {
                    host.send(json!({"method":"invalidate", "params":{"section":"play"}}))?;
                }
                Ok(Value::Null)
            }
            Some("summary") => Ok(Value::Null),
            _ => Err("Unsupported launcher request".into()),
        }
    }
}

fn validate_loader(path: &Path) -> Result<()> {
    let extension = path.extension();
    let batch = extension.is_some_and(|ext| ext.eq_ignore_ascii_case("bat"));
    let exe = extension.is_some_and(|ext| ext.eq_ignore_ascii_case("exe"));
    if !path.is_absolute() || !path.is_file() || !(batch || exe) {
        return Err("Choose an existing .bat or .exe loader file".into());
    }
    if batch && path.to_string_lossy().contains(['%', '\r', '\n', '"']) {
        return Err("The .bat loader path must have no percent signs or quotes".into());
    }
    Ok(())
}

fn main() -> Result<()> {
    let mut args = std::env::args_os().skip(1);
    if args.next().is_some_and(|arg| arg == "--apply-update") {
        let request = args.next().ok_or("Missing update request")?;
        return updates::apply(Path::new(&request));
    }
    let mut host = Host {
        input: io::stdin().lock(),
        output: io::stdout().lock(),
        next_id: 1,
    };
    let mut addon = Addon::default();
    while let Some(request) = host.read()? {
        let result = addon.call(&mut host, &request);
        if let Some(id) = request.get("id") {
            host.send(match result {
                Ok(value) => json!({"id":id, "result":value}),
                Err(error) => json!({"id":id, "error":error.to_string()}),
            })?;
        }
    }
    Ok(())
}
