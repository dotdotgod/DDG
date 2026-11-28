use std::{env, fs, path::PathBuf};

fn main() {
    let dir = "assets/icons/programs";
    println!("cargo:rerun-if-changed={dir}");
    let mut paths: Vec<_> = fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "svg"))
        .collect();
    paths.sort_unstable();
    let mut source = String::from(
        "fn program_asset(key: &str) -> Option<(&'static str, &'static [u8])> {\n    Some(match key {\n",
    );
    for path in paths {
        let key = path.file_stem().unwrap().to_str().unwrap();
        source.push_str(&format!(
            "        {key:?} => ({name:?}, include_bytes!(concat!(env!(\"CARGO_MANIFEST_DIR\"), {file:?}))),\n",
            name = format!("programs/{key}.svg"),
            file = format!("/{dir}/{key}.svg"),
        ));
    }
    source.push_str("        _ => return None,\n    })\n}\n");
    let out = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    fs::write(out.join("program_assets.rs"), source).unwrap();
}
