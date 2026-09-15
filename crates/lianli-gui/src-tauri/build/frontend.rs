use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Read};
use std::path::Path;

pub const INPUTS: &[&str] = &[
    "package.json",
    "package-lock.json",
    "vite.config.ts",
    "tsconfig.json",
    "tsconfig.node.json",
    "index.html",
    "src",
    "scripts",
];
pub const MANIFEST: &str = ".lianli-build.json";

fn collect(root: &Path, path: &Path, hashes: &mut BTreeMap<String, String>) -> io::Result<()> {
    let absolute = root.join(path);
    let metadata = fs::symlink_metadata(&absolute)?;
    if metadata.is_dir() {
        for entry in fs::read_dir(&absolute)? {
            collect(root, &path.join(entry?.file_name()), hashes)?;
        }
    } else if metadata.is_file() {
        let mut file = fs::File::open(absolute)?;
        let mut digest = Sha256::new();
        let mut buffer = [0; 8192];
        loop {
            let read = file.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            digest.update(&buffer[..read]);
        }
        hashes.insert(
            path.to_string_lossy().into_owned(),
            format!("{:x}", digest.finalize()),
        );
    } else {
        return Err(io::Error::other(format!(
            "Frontend input/output is not a regular file: {}",
            absolute.display()
        )));
    }
    Ok(())
}

pub fn verify(frontend: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let dist = frontend.join("dist");
    let manifest: serde_json::Value =
        serde_json::from_reader(fs::File::open(dist.join(MANIFEST))?)?;
    let mut inputs = BTreeMap::new();
    for input in INPUTS {
        collect(frontend, Path::new(input), &mut inputs)?;
    }
    match fs::symlink_metadata(frontend.join("public")) {
        Ok(_) => collect(frontend, Path::new("public"), &mut inputs)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let mut outputs = BTreeMap::new();
    for entry in fs::read_dir(&dist)? {
        let name = entry?.file_name();
        if name != MANIFEST {
            collect(&dist, Path::new(&name), &mut outputs)?;
        }
    }
    if !outputs.contains_key("index.html") || !outputs.keys().any(|path| path.ends_with(".js")) {
        return Err("Frontend build is missing HTML or JavaScript output".into());
    }
    if manifest != serde_json::json!({"inputs": inputs, "outputs": outputs}) {
        return Err("Frontend sources or output differ from the verified build manifest".into());
    }
    Ok(())
}
