use std::{
    io,
    path::{Path, PathBuf},
};

fn inside(root: &Path, path: &Path, config: &Path) -> io::Result<PathBuf> {
    let canonical = path.canonicalize()?;
    if canonical != config && canonical.starts_with(root.canonicalize()?) {
        Ok(canonical)
    } else {
        Err(io::Error::from(io::ErrorKind::NotFound))
    }
}

pub(crate) fn files(root: PathBuf, config: PathBuf) -> actix_files::Files {
    actix_files::Files::new("/", &root)
        .index_file("index.html")
        .path_filter(move |relative, _| {
            let candidate = root.join(relative);
            let file = if candidate.is_dir() {
                candidate.join("index.html")
            } else {
                candidate
            };
            inside(&root, &file, &config).is_ok()
        })
}

pub(crate) fn open_index(path: &Path, config: &Path) -> io::Result<actix_files::NamedFile> {
    actix_files::NamedFile::open(inside(
        path.parent().ok_or(io::ErrorKind::NotFound)?,
        path,
        config,
    )?)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::{fs, os::unix::fs::symlink};
    #[actix_web::test]
    async fn links_cannot_serve_external_files_or_indexes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("static");
        fs::create_dir(&root).unwrap();
        let secret = dir.path().join("config.toml");
        fs::write(&secret, "private").unwrap();
        symlink(&secret, root.join("asset.txt")).unwrap();
        symlink(&secret, root.join("index.html")).unwrap();
        let app = actix_web::test::init_service(
            actix_web::App::new().service(files(root.clone(), secret.canonicalize().unwrap())),
        )
        .await;
        for path in ["/asset.txt", "/"] {
            let response = actix_web::test::call_service(
                &app,
                actix_web::test::TestRequest::get().uri(path).to_request(),
            )
            .await;
            assert!(!response.status().is_success());
        }
        assert!(open_index(&root.join("index.html"), &secret).is_err());
    }
}
