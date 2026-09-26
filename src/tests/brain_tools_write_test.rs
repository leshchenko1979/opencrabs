use crate::brain::tools::Tool;
use crate::brain::tools::ToolCapability;
use crate::brain::tools::ToolExecutionContext;
use crate::brain::tools::write::*;
use tempfile::TempDir;
use tokio;
use uuid::Uuid;

#[tokio::test]
async fn test_write_file() {
    let temp_dir = TempDir::new().unwrap();
    let file_path = temp_dir.path().join("test.txt");

    let tool = WriteTool;
    let session_id = Uuid::new_v4();
    let context =
        ToolExecutionContext::new(session_id).with_working_directory(temp_dir.path().to_path_buf());

    let input = serde_json::json!({
        "path": "test.txt",
        "content": "Hello, World!"
    });

    let result = tool.execute(input, &context).await.unwrap();
    assert!(result.success);

    // Verify file was written
    let contents = tokio::fs::read_to_string(&file_path).await.unwrap();
    assert_eq!(contents, "Hello, World!");
}

#[tokio::test]
async fn test_write_file_with_create_dirs() {
    let temp_dir = TempDir::new().unwrap();
    let file_path = temp_dir.path().join("subdir").join("test.txt");

    let tool = WriteTool;
    let session_id = Uuid::new_v4();
    let context =
        ToolExecutionContext::new(session_id).with_working_directory(temp_dir.path().to_path_buf());

    let input = serde_json::json!({
        "path": "subdir/test.txt",
        "content": "Nested file",
        "create_dirs": true
    });

    let result = tool.execute(input, &context).await.unwrap();
    assert!(result.success);

    // Verify file was written
    let contents = tokio::fs::read_to_string(&file_path).await.unwrap();
    assert_eq!(contents, "Nested file");
}

#[tokio::test]
async fn test_write_file_missing_parent_dir() {
    let temp_dir = TempDir::new().unwrap();

    let tool = WriteTool;
    let session_id = Uuid::new_v4();
    let context =
        ToolExecutionContext::new(session_id).with_working_directory(temp_dir.path().to_path_buf());

    let input = serde_json::json!({
        "path": "nonexistent/test.txt",
        "content": "Should fail",
        "create_dirs": false
    });

    let result = tool.execute(input, &context).await.unwrap();
    assert!(!result.success);
    assert!(result.error.is_some());
}

#[test]
fn test_write_tool_schema() {
    let tool = WriteTool;
    assert_eq!(tool.name(), "write_file");
    assert!(tool.requires_approval());

    let capabilities = tool.capabilities();
    assert!(capabilities.contains(&ToolCapability::WriteFiles));
    assert!(capabilities.contains(&ToolCapability::SystemModification));
}

#[tokio::test]
async fn test_overwrite_existing_file() {
    let temp_dir = TempDir::new().unwrap();
    let file_path = temp_dir.path().join("test.txt");

    // Write initial content
    tokio::fs::write(&file_path, "Initial content")
        .await
        .unwrap();

    let tool = WriteTool;
    let session_id = Uuid::new_v4();
    let context =
        ToolExecutionContext::new(session_id).with_working_directory(temp_dir.path().to_path_buf());

    let input = serde_json::json!({
        "path": "test.txt",
        "content": "New content",
        "overwrite_read_confirm": true
    });

    let result = tool.execute(input, &context).await.unwrap();
    assert!(result.success);

    // Verify file was overwritten
    let contents = tokio::fs::read_to_string(&file_path).await.unwrap();
    assert_eq!(contents, "New content");
}

/// Issue #609: a `create_dirs: true` write whose parent directory ALREADY
/// exists outside the working directory must succeed.
///
/// The removed guard fired only when the parent already existed, so this arm
/// was refused while the fresh-parent arm (`test_write_file_with_create_dirs`)
/// passed. Both arms must now behave alike.
#[tokio::test]
async fn test_write_file_with_create_dirs_into_existing_dir_outside_working_dir() {
    let wd = TempDir::new().unwrap();
    let target = TempDir::new().unwrap();
    let subdir = target.path().join("subdir");
    // Pre-create the parent so the pre-fix guard's `parent.exists()` arm fires.
    tokio::fs::create_dir_all(&subdir).await.unwrap();
    let file_path = subdir.join("test.txt");

    let tool = WriteTool;
    let session_id = Uuid::new_v4();
    let context =
        ToolExecutionContext::new(session_id).with_working_directory(wd.path().to_path_buf());

    let input = serde_json::json!({
        "path": file_path.to_string_lossy(),
        "content": "Existing dir write",
        "create_dirs": true
    });

    let result = tool.execute(input, &context).await.unwrap();
    assert!(
        result.success,
        "write into an existing directory outside the working dir must succeed (#609); error={:?}",
        result.error
    );

    let contents = tokio::fs::read_to_string(&file_path).await.unwrap();
    assert_eq!(contents, "Existing dir write");
}
