use super::*;
use std::cell::RefCell;

#[derive(Default)]
struct RecordingSupervisor {
    calls: RefCell<Vec<String>>,
}
impl Supervisor for RecordingSupervisor {
    fn install(&self, spec: &ServiceSpec) -> Result<(), SupervisorError> {
        self.calls.borrow_mut().push(format!("install:{}", spec.name));
        Ok(())
    }
    fn start(&self, name: &str) -> Result<(), SupervisorError> {
        self.calls.borrow_mut().push(format!("start:{name}"));
        Ok(())
    }
    fn stop(&self, name: &str) -> Result<(), SupervisorError> {
        self.calls.borrow_mut().push(format!("stop:{name}"));
        Ok(())
    }
    fn uninstall(&self, name: &str) -> Result<(), SupervisorError> {
        self.calls.borrow_mut().push(format!("uninstall:{name}"));
        Ok(())
    }
    fn status_with_word(
        &self,
        _name: &str,
    ) -> Result<(ServiceStatus, Option<String>), SupervisorError> {
        Ok((ServiceStatus::Active, None))
    }
}

fn spec(name: &str) -> ServiceSpec {
    ServiceSpec {
        name: name.into(),
        program: std::path::PathBuf::from("/bin/true"),
        args: vec![],
        env: vec![],
        working_dir: None,
        keep_alive: true,
        stdout_log: None,
        stderr_log: None,
        after: vec![],
        part_of: Some("kastellan".into()),
        restart_backoff: None,
        environment_files: Vec::new(),
    }
}

#[test]
fn default_bundle_installs_then_starts_in_member_order() {
    let sup = RecordingSupervisor::default();
    let target = TargetSpec {
        name: "kastellan".into(),
        members: vec!["kastellan-postgres".into(), "kastellan-core".into()],
    };
    let members = [spec("kastellan-postgres"), spec("kastellan-core")];
    sup.install_target(&target, &members).unwrap();
    sup.start_target(&target).unwrap();
    let calls = sup.calls.borrow().clone();
    assert_eq!(
        calls,
        vec![
            "install:kastellan-postgres",
            "install:kastellan-core",
            "start:kastellan-postgres",
            "start:kastellan-core",
        ]
    );
}

#[test]
fn default_bundle_stops_in_reverse_member_order() {
    let sup = RecordingSupervisor::default();
    let target = TargetSpec {
        name: "kastellan".into(),
        members: vec!["kastellan-postgres".into(), "kastellan-core".into()],
    };
    sup.stop_target(&target).unwrap();
    assert_eq!(
        sup.calls.borrow().clone(),
        vec!["stop:kastellan-core", "stop:kastellan-postgres"]
    );
}

#[test]
fn default_bundle_uninstalls_in_reverse_member_order() {
    let sup = RecordingSupervisor::default();
    let target = TargetSpec {
        name: "kastellan".into(),
        members: vec!["kastellan-postgres".into(), "kastellan-core".into()],
    };
    sup.uninstall_target(&target).unwrap();
    assert_eq!(
        sup.calls.borrow().clone(),
        vec!["uninstall:kastellan-core", "uninstall:kastellan-postgres"]
    );
}
