#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    #[cfg(feature = "mcp-harness")]
    if std::env::args_os()
        .skip(1)
        .any(|arg| arg == "--mcp-harness")
    {
        if let Err(error) = chena_shell_lib::run_mcp_harness() {
            eprintln!("Chena-Shell MCP harness error: {error}");
            std::process::exit(1);
        }
        return;
    }
    if std::env::args_os().skip(1).any(|arg| arg == "--mcp-stdio") {
        if let Err(error) = chena_shell_lib::run_mcp_stdio() {
            eprintln!("Chena-Shell MCP stdio error: {error}");
            std::process::exit(1);
        }
        return;
    }
    chena_shell_lib::run();
}
