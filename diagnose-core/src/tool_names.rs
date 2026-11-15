//! Names of Provider tools that runtime code refers to outside their own
//! registration. Tools that already own a name constant keep it, such as
//! `web_research::WEB_SEARCH_TOOL_NAME` or `exec_tools::EXEC_TOOL_NAME`.

pub const CONVERT_DOCUMENT: &str = "convert_document";
pub const CREATE_TEXT_FILE: &str = "create_text_file";
pub const INSPECT_DESKTOP_SESSION: &str = "inspect_desktop_session";
pub const INSPECT_DESKTOP_UI: &str = "inspect_desktop_ui";
pub const INSPECT_FILES: &str = "inspect_files";
pub const INSPECT_SPREADSHEETS: &str = "inspect_spreadsheets";
pub const PREVIEW_DOCUMENT: &str = "preview_document";
pub const PREVIEW_SPREADSHEET_MERGE: &str = "preview_spreadsheet_merge";
pub const READ_CURRENT_SCREEN: &str = "read_current_screen";
pub const READ_TEXT_FILE: &str = "read_text_file";
