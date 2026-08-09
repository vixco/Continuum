use std::path::Path;

const AGENT_OS_NAME: &str = "agent-os";
const AGENT_OS_BIN: &str = if cfg!(windows) {
    "continuum-agent-os.exe"
} else {
    "continuum-agent-os"
};

/// Register the release-bundled Agent OS as a local stdio MCP server. The
/// operation is idempotent and never downloads or executes code: it only
/// records the already-bundled executable for the next agent run.
pub(crate) fn ensure_bundled(data_dir: &Path) -> Result<bool, String> {
    let installed = continuum_core::mcp_registry::list_servers(data_dir)
        .map_err(|error| format!("Could not inspect MCP registrations: {error}"))?;
    if installed.iter().any(|server| server.name == AGENT_OS_NAME) {
        return Ok(false);
    }

    let Some(binary) = crate::commands::bundled_binary_candidates(AGENT_OS_BIN)
        .into_iter()
        .find(|candidate| candidate.is_file())
    else {
        tracing::debug!(
            layer = "desktop",
            component = "agent_os_registration",
            binary = AGENT_OS_BIN,
            "Bundled Agent OS binary is not present in this development build"
        );
        return Ok(false);
    };

    continuum_core::mcp_registry::install_server(
        data_dir,
        AGENT_OS_NAME,
        &binary.to_string_lossy(),
        vec![
            "--data-dir".to_string(),
            data_dir.to_string_lossy().into_owned(),
        ],
    )
    .map_err(|error| format!("Could not register bundled Agent OS: {error}"))?;

    tracing::info!(
        layer = "desktop",
        component = "agent_os_registration",
        binary = %binary.display(),
        "Registered the bundled governed Agent OS for future agent runs"
    );
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn existing_registration_is_preserved_without_requiring_a_bundle() {
        let temp = tempfile::tempdir().expect("tempdir");
        let executable = temp.path().join(if cfg!(windows) {
            "agent-os-fixture.exe"
        } else {
            "agent-os-fixture"
        });
        std::fs::write(&executable, b"fixture").expect("fixture executable");
        continuum_core::mcp_registry::install_server(
            temp.path(),
            AGENT_OS_NAME,
            executable.to_str().expect("utf8 path"),
            vec![
                "--data-dir".into(),
                temp.path().to_string_lossy().into_owned(),
            ],
        )
        .expect("seed registration");

        assert!(!ensure_bundled(temp.path()).expect("idempotent registration"));
        let servers = continuum_core::mcp_registry::list_servers(temp.path()).expect("list");
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].name, AGENT_OS_NAME);
        assert_eq!(
            servers[0].command,
            executable
                .canonicalize()
                .expect("canonical fixture")
                .to_string_lossy()
                .into_owned()
        );
    }
}
