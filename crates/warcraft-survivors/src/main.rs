//! The `warcraft-survivors` launcher: the build stamp, handed to the survivors mode.

use benilla_app::BuildId;

fn main() -> benilla_app::AppExit {
    #[cfg(target_os = "windows")]
    if std::env::var_os("WGPU_BACKEND").is_none() {
        std::env::set_var("WGPU_BACKEND", "gl");
    }

    if std::env::var_os("WOW_NO_PIPE_WARM").is_none() {
        std::env::set_var("WOW_NO_PIPE_WARM", "1");
    }
    if std::env::var_os("WOW_NO_SSAO").is_none() {
        std::env::set_var("WOW_NO_SSAO", "1");
    }
    if std::env::var_os("WOW_STATIC_GX").is_none() {
        std::env::set_var("WOW_STATIC_GX", "0");
    }
    if std::env::var_os("WOW_STATIC_MERGE").is_none() {
        std::env::set_var("WOW_STATIC_MERGE", "0");
    }
    if std::env::var_os("WOW_NO_DOODAD_BODIES").is_none() {
        std::env::set_var("WOW_NO_DOODAD_BODIES", "1");
    }
    if std::env::var_os("WOW_CLUTTER_DENSITY").is_none() {
        std::env::set_var("WOW_CLUTTER_DENSITY", "0");
    }

    let args: Vec<String> = std::env::args().collect();
    let mut i = 1;
    let mut window_arg_set = false;
    while i < args.len() {
        if args[i] == "--data" {
            if let Some(d) = args.get(i + 1) {
                std::env::set_var("WOW_DATA", d);
                i += 1;
            }
        } else if let Some(d) = args[i].strip_prefix("--data=") {
            std::env::set_var("WOW_DATA", d);
        } else if args[i] == "--window" {
            if let Some(res) = args.get(i + 1) {
                std::env::set_var("WOW_WIN", res);
                window_arg_set = true;
                i += 1;
            }
        } else if let Some(res) = args[i].strip_prefix("--window=") {
            std::env::set_var("WOW_WIN", res);
            window_arg_set = true;
        } else if args[i] == "--fullscreen" {
            std::env::remove_var("WOW_WIN");
            window_arg_set = true;
        }
        i += 1;
    }

    if !window_arg_set && std::env::var_os("WOW_WIN").is_none() {
        // Default to 1024x768 windowed on Windows to avoid excessive rasterization load
        // on software / low-end adapters and avoid DXGI mode-switch suspension.
        #[cfg(target_os = "windows")]
        std::env::set_var("WOW_WIN", "1024x768");
    }

    if std::env::var_os("WOW_DATA").is_none() {
        let mut candidates = Vec::new();
        candidates.push(std::path::Path::new(env!("BENILLA_PROJECT_DIR")).join("wow-data/Data"));
        candidates.push(std::path::Path::new(env!("BENILLA_PROJECT_DIR")).join("WoW/Data"));
        candidates.push(std::path::PathBuf::from("wow-data/Data"));
        candidates.push(std::path::PathBuf::from("WoW/Data"));
        candidates.push(std::path::PathBuf::from("Data"));
        if let Ok(exe) = std::env::current_exe() {
            if let Some(dir) = exe.parent() {
                candidates.push(dir.join("wow-data/Data"));
                candidates.push(dir.join("WoW/Data"));
                candidates.push(dir.join("Data"));
                if let Some(parent) = dir.parent() {
                    candidates.push(parent.join("wow-data/Data"));
                    candidates.push(parent.join("WoW/Data"));
                    candidates.push(parent.join("Data"));
                }
            }
        }
        for cand in candidates {
            if cand.is_dir() && (cand.join("terrain.MPQ").is_file() || cand.join("terrain.mpq").is_file()) {
                std::env::set_var("WOW_DATA", cand);
                break;
            }
        }
    }

    benilla_app::run_survivors(BuildId {
        version: env!("CARGO_PKG_VERSION"),
        describe: env!("BENILLA_GIT_DESCRIBE"),
        sha: env!("BENILLA_GIT_SHA"),
        short: env!("BENILLA_GIT_SHORT"),
        date: env!("BENILLA_GIT_DATE"),
        profile: env!("BENILLA_PROFILE"),
        project_dir: env!("BENILLA_PROJECT_DIR"),
        ..Default::default()
    })
}
