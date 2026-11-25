#[path = "../render/engine.rs"]
mod engine;
#[path = "../render/vector.rs"]
mod vector;
#[path = "../render/assets_gen.rs"]
mod assets_gen;

use std::path::PathBuf;

fn parse_args<I, S>(args: I) -> Result<bool, String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut overwrite_scenes = false;
    for arg in args {
        match arg.as_ref() {
            "--overwrite-scenes" => overwrite_scenes = true,
            other => return Err(format!("unknown argument: {other}")),
        }
    }
    Ok(overwrite_scenes)
}

fn main() {
    let overwrite_scenes = match parse_args(std::env::args().skip(1)) {
        Ok(flag) => flag,
        Err(err) => {
            eprintln!("[gen_assets] {err}");
            eprintln!("usage: gen_assets [--overwrite-scenes]");
            std::process::exit(2);
        }
    };
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("assets");
    match engine::Engine::new().and_then(|engine| assets_gen::generate(&engine, &root, overwrite_scenes)) {
        Ok(()) => println!("[gen_assets] assets written to {}", root.display()),
        Err(err) => {
            eprintln!("[gen_assets] failed: {err:?}");
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::parse_args;

    #[test]
    fn scene_overwrite_flag_is_opt_in() {
        assert!(!parse_args(std::iter::empty::<&str>()).unwrap());
        assert!(parse_args(["--overwrite-scenes"]).unwrap());
        assert!(parse_args(["invalid"]).is_err());
    }
}
