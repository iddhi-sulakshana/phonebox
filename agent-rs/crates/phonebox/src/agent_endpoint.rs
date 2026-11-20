//! Port of `AgentEndpoint.cs`: a separate local port allows development checks beside an installed agent.

/// The local port the agent listens on (`--port`, default 8765).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgentEndpoint {
    pub port: u16,
}

pub const DEFAULT_PORT: u16 = 8765;
pub const PORT_ERROR: &str = "--port requires one integer between 1024 and 65535";

impl Default for AgentEndpoint {
    fn default() -> Self {
        AgentEndpoint { port: DEFAULT_PORT }
    }
}

impl AgentEndpoint {
    pub fn settings_url(&self) -> String {
        format!("http://127.0.0.1:{}/", self.port)
    }

    /// The single-instance name, as the C# agent's mutex: `Local\PhoneBox.Agent`, or `Local\PhoneBox.Agent.<port>`.
    pub fn mutex_name(&self) -> String {
        if self.port == DEFAULT_PORT {
            r"Local\PhoneBox.Agent".to_string()
        } else {
            format!(r"Local\PhoneBox.Agent.{}", self.port)
        }
    }

    /// Reads `--port <n>`; anything but exactly one integer in 1024..=65535 is an error ([`PORT_ERROR`]).
    pub fn from_args(args: &[String]) -> Result<AgentEndpoint, String> {
        let positions: Vec<usize> = args
            .iter()
            .enumerate()
            .filter(|(_, a)| *a == "--port")
            .map(|(i, _)| i)
            .collect();
        let Some(&index) = positions.first() else {
            return Ok(AgentEndpoint::default());
        };
        // int.TryParse allows surrounding white space
        let port = args
            .get(index + 1)
            .and_then(|v| v.trim().parse::<i64>().ok());
        match port {
            Some(p) if positions.len() == 1 && (1024..=65535).contains(&p) => {
                Ok(AgentEndpoint { port: p as u16 })
            }
            _ => Err(PORT_ERROR.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn defaults_and_isolated_instances_have_distinct_urls_and_mutexes() {
        let standard = AgentEndpoint::from_args(&args(&[])).unwrap();
        let isolated =
            AgentEndpoint::from_args(&args(&["--config", "test.json", "--port", "18765"])).unwrap();
        assert_eq!(standard.port, 8765);
        assert_eq!(standard.mutex_name(), r"Local\PhoneBox.Agent");
        assert_eq!(isolated.settings_url(), "http://127.0.0.1:18765/");
        assert_ne!(standard.mutex_name(), isolated.mutex_name());
    }

    #[test]
    fn invalid_ports_are_rejected() {
        for value in ["0", "65536", "not-a-port", "8765; command", "1023"] {
            assert_eq!(
                AgentEndpoint::from_args(&args(&["--port", value])),
                Err(PORT_ERROR.to_string()),
                "{value}"
            );
        }
    }

    #[test]
    fn missing_and_repeated_ports_are_rejected() {
        assert!(AgentEndpoint::from_args(&args(&["--port"])).is_err());
        assert!(AgentEndpoint::from_args(&args(&["--port", "18765", "--port", "18766"])).is_err());
    }
}
