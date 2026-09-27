use solar_controller::config::{self, Config};

fn load_str(content: &str) -> anyhow::Result<Config> {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(&path, content).unwrap();
    config::load(path.to_str().unwrap())
}

#[test]
fn example_config_parses_and_directives_path_defaults() {
    let cfg = config::load("config.example.toml").unwrap();
    assert_eq!(cfg.port, "/dev/ttyRS485");
    assert_eq!(cfg.baud, 9600);
    assert_eq!(cfg.slave, 1);
    assert_eq!(cfg.interval_secs, 5);
    assert_eq!(cfg.bind, "0.0.0.0:8300");
    assert_eq!(cfg.token, "");
    assert_eq!(cfg.directives_path, "directives.json");
}

#[test]
fn directives_path_is_honored_when_present() {
    let cfg = load_str(
        "port=\"/dev/x\"\nbaud=9600\nslave=1\ninterval_secs=5\nbind=\"0.0.0.0:8300\"\ntoken=\"t\"\ndirectives_path=\"/var/lib/d.json\"\n",
    )
    .unwrap();
    assert_eq!(cfg.directives_path, "/var/lib/d.json");
}

#[test]
fn unknown_field_rejected() {
    assert!(load_str(
        "port=\"/dev/x\"\nbaud=9600\nslave=1\ninterval_secs=5\nbind=\"b\"\ntoken=\"t\"\nretries=9\n"
    )
    .is_err());
}

#[test]
fn missing_required_field_rejected() {
    assert!(load_str("port=\"/dev/x\"\nbaud=9600\nslave=1\ninterval_secs=5\nbind=\"b\"\n").is_err());
}

#[test]
fn wrong_type_rejected() {
    assert!(load_str(
        "port=\"/dev/x\"\nbaud=\"fast\"\nslave=1\ninterval_secs=5\nbind=\"b\"\ntoken=\"t\"\n"
    )
    .is_err());
}
