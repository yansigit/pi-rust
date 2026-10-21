use rpi_tools::{Durability, FileContent, FileSystem, InMemoryExecutionEnv, OsExecutionEnv};

#[tokio::test]
async fn durability_requires_an_existing_regular_file_and_distinguishes_memory() {
    let directory = tempfile::tempdir().unwrap();
    let os = OsExecutionEnv::with_cwd(directory.path().to_owned());
    let memory = InMemoryExecutionEnv::new();
    for (fs, expected) in [
        (&os as &dyn FileSystem, Durability::Durable),
        (&memory as &dyn FileSystem, Durability::Ephemeral),
    ] {
        assert!(fs.sync_file("missing").await.is_err());
        fs.write_file(
            "nested/file",
            FileContent::Text("durable data".into()),
            None,
        )
        .await
        .unwrap();
        assert_eq!(fs.sync_file("nested/file").await.unwrap(), expected);
        assert_eq!(
            fs.read_text_file("nested/file", None).await.unwrap(),
            "durable data"
        );
        assert!(fs.sync_file("nested").await.is_err());
    }
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(
            directory.path().join("nested/file"),
            directory.path().join("link"),
        )
        .unwrap();
        assert!(os.sync_file("link").await.is_err());
    }
}
