use std::fs;
use std::path::Path;

#[cfg(target_os = "linux")]
fn read_cmdline_at(base: &Path, pid: u32) -> Option<Vec<String>> {
    let path = base.join(pid.to_string()).join("cmdline");
    let data = fs::read(&path).ok()?;
    let args: Vec<String> = data
        .split(|&b| b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .collect();
    if args.is_empty() { None } else { Some(args) }
}

#[cfg(target_os = "linux")]
fn read_cwd_at(base: &Path, pid: u32) -> Option<String> {
    let path = base.join(pid.to_string()).join("cwd");
    fs::read_link(&path)
        .ok()
        .and_then(|p| p.into_os_string().into_string().ok())
}

#[cfg(target_os = "linux")]
fn read_comm_at(base: &Path, pid: u32) -> Option<String> {
    let path = base.join(pid.to_string()).join("comm");
    fs::read_to_string(&path).ok().map(|s| s.trim().to_string())
}

#[cfg(target_os = "linux")]
fn get_children_at(base: &Path, pid: u32) -> Vec<u32> {
    let path = base
        .join(pid.to_string())
        .join("task")
        .join(pid.to_string())
        .join("children");
    fs::read_to_string(&path)
        .ok()
        .map(|s| {
            s.split_whitespace()
                .filter_map(|p| p.parse::<u32>().ok())
                .collect()
        })
        .unwrap_or_default()
}

fn is_shell(comm: &str, shell_names: &[String]) -> bool {
    shell_names.iter().any(|s| s == comm)
}

fn is_helper(comm: &str, helper_names: &[String]) -> bool {
    helper_names.iter().any(|s| s == comm)
}

#[cfg(target_os = "linux")]
fn read_stat_field_at(base: &Path, pid: u32, field: usize) -> Option<i64> {
    let path = base.join(pid.to_string()).join("stat");
    let data = fs::read_to_string(&path).ok()?;

    let comm_end = data.find(')')?;
    let after_comm = &data[comm_end + 2..];
    let fields: Vec<&str> = after_comm.split_whitespace().collect();

    let idx = field.saturating_sub(3);
    fields.get(idx).and_then(|f| f.parse::<i64>().ok())
}

#[cfg(target_os = "linux")]
fn resolve_child_process_at(
    base: &Path,
    pid: u32,
    shell_names: &[String],
    helper_names: &[String],
    max_depth: u32,
) -> Option<(Vec<String>, String)> {
    let mut current = pid;

    for depth in 0..max_depth {
        let proc_path = base.join(current.to_string());
        if !proc_path.exists() {
            if depth == 0 {
                eprintln!("Warning: [proc] PID {} no longer exists in {:?}", pid, base);
            }
            break;
        }

        let children = get_children_at(base, current);
        let tpgid = read_stat_field_at(base, current, 8).unwrap_or(0) as u32;

        if children.is_empty() {
            if tpgid > 0
                && let Some(fg_comm) = read_comm_at(base, tpgid)
                && !is_shell(&fg_comm, shell_names)
                && fg_comm != "__atexit__"
                && !is_helper(&fg_comm, helper_names)
            {
                let cmd = read_cmdline_at(base, tpgid).unwrap_or_default();
                let cwd = read_cwd_at(base, tpgid).unwrap_or_default();
                return Some((cmd, cwd));
            }
            break;
        }

        // Prefer the foreground child (matching tpgid); fall back to first child.
        let next_pid = children
            .iter()
            .copied()
            .find(|&c| tpgid > 0 && c == tpgid)
            .unwrap_or(children[0]);

        let comm = match read_comm_at(base, next_pid) {
            Some(c) => c,
            None => {
                eprintln!(
                    "Warning: [proc] could not read comm for PID {} (child of {})",
                    next_pid, current
                );
                return None;
            }
        };

        if is_shell(&comm, shell_names) || is_helper(&comm, helper_names) {
            current = next_pid;
            continue;
        }

        let cmd = read_cmdline_at(base, next_pid).unwrap_or_default();
        let cwd = read_cwd_at(base, next_pid).unwrap_or_default();
        return Some((cmd, cwd));
    }

    None
}

#[cfg(target_os = "linux")]
pub fn resolve_child_process(
    pid: u32,
    shell_names: &[String],
    helper_names: &[String],
    max_depth: u32,
) -> Option<(Vec<String>, String)> {
    resolve_child_process_at(
        Path::new("/proc"),
        pid,
        shell_names,
        helper_names,
        max_depth,
    )
}

#[cfg(not(target_os = "linux"))]
pub fn resolve_child_process(
    _pid: u32,
    _shell_names: &[String],
    _helper_names: &[String],
    _max_depth: u32,
) -> Option<(Vec<String>, String)> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::symlink;
    use std::path::PathBuf;

    fn create_fake_proc_dir(dir: &Path, pid: u32) -> PathBuf {
        let pid_dir = dir.join(pid.to_string());
        fs::create_dir_all(&pid_dir).unwrap();
        let task_dir = pid_dir.join("task").join(pid.to_string());
        fs::create_dir_all(&task_dir).unwrap();
        pid_dir
    }

    fn write_cmdline(dir: &Path, args: &[&str]) {
        let data: Vec<u8> = args
            .iter()
            .flat_map(|a| {
                let mut bytes = a.as_bytes().to_vec();
                bytes.push(0);
                bytes
            })
            .collect();
        fs::write(dir.join("cmdline"), data).unwrap();
    }

    fn write_comm(dir: &Path, name: &str) {
        fs::write(dir.join("comm"), format!("{}\n", name)).unwrap();
    }

    fn write_children(dir: &Path, pids: &[u32]) {
        let children_path = dir
            .join("task")
            .join(dir.file_name().unwrap())
            .join("children");
        let content: String = pids.iter().map(|p| format!("{} ", p)).collect();
        fs::write(children_path, content.trim()).unwrap();
    }

    #[test]
    fn resolve_finds_direct_child() {
        let tmp = tempfile::tempdir().unwrap();

        let kitty_dir = create_fake_proc_dir(tmp.path(), 1000);
        write_cmdline(&kitty_dir, &["kitty"]);
        write_comm(&kitty_dir, "kitty");
        write_children(&kitty_dir, &[1001]);

        let fish_dir = create_fake_proc_dir(tmp.path(), 1001);
        write_cmdline(&fish_dir, &["fish"]);
        write_comm(&fish_dir, "fish");
        write_children(&fish_dir, &[1002]);

        let btop_dir = create_fake_proc_dir(tmp.path(), 1002);
        write_cmdline(&btop_dir, &["btop"]);
        write_comm(&btop_dir, "btop");
        symlink("/home/user", btop_dir.join("cwd")).unwrap();
        write_children(&btop_dir, &[]);

        let shell_names = vec!["fish".to_string(), "bash".to_string()];
        let helper_names: Vec<String> = vec![];
        let result = resolve_child_process_at(tmp.path(), 1000, &shell_names, &helper_names, 20);
        assert_eq!(
            result,
            Some((vec!["btop".to_string()], "/home/user".to_string()))
        );
    }

    #[test]
    fn resolve_skips_shell_and_finds_grandchild() {
        let tmp = tempfile::tempdir().unwrap();

        let kitty_dir = create_fake_proc_dir(tmp.path(), 1000);
        write_cmdline(&kitty_dir, &["kitty"]);
        write_comm(&kitty_dir, "kitty");
        write_children(&kitty_dir, &[1001]);

        let bash_dir = create_fake_proc_dir(tmp.path(), 1001);
        write_cmdline(&bash_dir, &["bash"]);
        write_comm(&bash_dir, "bash");
        write_children(&bash_dir, &[1002]);

        let nvim_dir = create_fake_proc_dir(tmp.path(), 1002);
        write_cmdline(&nvim_dir, &["nvim", "/path/to/file"]);
        write_comm(&nvim_dir, "nvim");
        symlink("/home/user/projects", nvim_dir.join("cwd")).unwrap();
        write_children(&nvim_dir, &[]);

        let shell_names = vec!["bash".to_string()];
        let helper_names: Vec<String> = vec![];
        let result = resolve_child_process_at(tmp.path(), 1000, &shell_names, &helper_names, 20);
        assert_eq!(
            result,
            Some((
                vec!["nvim".to_string(), "/path/to/file".to_string()],
                "/home/user/projects".to_string()
            ))
        );
    }

    #[test]
    fn resolve_skips_helpers() {
        let tmp = tempfile::tempdir().unwrap();

        let kitty_dir = create_fake_proc_dir(tmp.path(), 1000);
        write_cmdline(&kitty_dir, &["kitty"]);
        write_comm(&kitty_dir, "kitty");
        write_children(&kitty_dir, &[1001]);

        let kitten_dir = create_fake_proc_dir(tmp.path(), 1001);
        write_cmdline(&kitten_dir, &["kitten", "@", "ssh"]);
        write_comm(&kitten_dir, "kitten");
        write_children(&kitten_dir, &[1002]);

        let btop_dir = create_fake_proc_dir(tmp.path(), 1002);
        write_cmdline(&btop_dir, &["btop"]);
        write_comm(&btop_dir, "btop");
        symlink("/home/user", btop_dir.join("cwd")).unwrap();
        write_children(&btop_dir, &[]);

        let shell_names: Vec<String> = vec![];
        let helper_names = vec!["kitten".to_string()];
        let result = resolve_child_process_at(tmp.path(), 1000, &shell_names, &helper_names, 20);
        assert_eq!(
            result,
            Some((vec!["btop".to_string()], "/home/user".to_string()))
        );
    }

    #[test]
    fn resolve_returns_none_when_only_shell() {
        let tmp = tempfile::tempdir().unwrap();

        let kitty_dir = create_fake_proc_dir(tmp.path(), 1000);
        write_cmdline(&kitty_dir, &["kitty"]);
        write_comm(&kitty_dir, "kitty");
        write_children(&kitty_dir, &[1001]);

        let fish_dir = create_fake_proc_dir(tmp.path(), 1001);
        write_cmdline(&fish_dir, &["fish"]);
        write_comm(&fish_dir, "fish");
        write_children(&fish_dir, &[]);

        let shell_names = vec!["fish".to_string()];
        let helper_names: Vec<String> = vec![];
        let result = resolve_child_process_at(tmp.path(), 1000, &shell_names, &helper_names, 20);
        assert!(result.is_none());
    }

    #[test]
    fn resolve_returns_none_when_pid_missing() {
        let tmp = tempfile::tempdir().unwrap();

        let shell_names = vec!["fish".to_string()];
        let helper_names: Vec<String> = vec![];
        let result = resolve_child_process_at(tmp.path(), 9999, &shell_names, &helper_names, 20);
        assert!(result.is_none());
    }

    #[test]
    fn resolve_filters_atexit() {
        let tmp = tempfile::tempdir().unwrap();

        let kitty_dir = create_fake_proc_dir(tmp.path(), 1000);
        write_cmdline(&kitty_dir, &["kitty"]);
        write_comm(&kitty_dir, "kitty");
        write_children(&kitty_dir, &[1001]);

        let fish_dir = create_fake_proc_dir(tmp.path(), 1001);
        write_cmdline(&fish_dir, &["fish"]);
        write_comm(&fish_dir, "fish");
        write_children(&fish_dir, &[]);

        let atexit_stat = "1001 (fish) S 0 0 0 0 -1 1002\n";
        fs::write(fish_dir.join("stat"), atexit_stat).unwrap();

        let atexit_dir = create_fake_proc_dir(tmp.path(), 1002);
        write_cmdline(&atexit_dir, &["__atexit__"]);
        write_comm(&atexit_dir, "__atexit__");
        symlink("/home/user", atexit_dir.join("cwd")).unwrap();

        let shell_names = vec!["fish".to_string()];
        let helper_names: Vec<String> = vec![];
        let result = resolve_child_process_at(tmp.path(), 1000, &shell_names, &helper_names, 20);
        assert!(result.is_none());
    }

    #[test]
    fn resolve_uses_tpgid_fallback() {
        let tmp = tempfile::tempdir().unwrap();

        let kitty_dir = create_fake_proc_dir(tmp.path(), 1000);
        write_cmdline(&kitty_dir, &["kitty"]);
        write_comm(&kitty_dir, "kitty");
        write_children(&kitty_dir, &[1001]);

        let fish_dir = create_fake_proc_dir(tmp.path(), 1001);
        write_cmdline(&fish_dir, &["fish"]);
        write_comm(&fish_dir, "fish");
        write_children(&fish_dir, &[]);

        let stat_content = "1001 (fish) S 0 0 0 0 2000\n";
        fs::write(fish_dir.join("stat"), stat_content).unwrap();

        let btop_dir = create_fake_proc_dir(tmp.path(), 2000);
        write_cmdline(&btop_dir, &["btop"]);
        write_comm(&btop_dir, "btop");
        symlink("/home/user", btop_dir.join("cwd")).unwrap();

        let shell_names = vec!["fish".to_string()];
        let helper_names: Vec<String> = vec![];
        let result = resolve_child_process_at(tmp.path(), 1000, &shell_names, &helper_names, 20);
        assert_eq!(
            result,
            Some((vec!["btop".to_string()], "/home/user".to_string()))
        );
    }

    #[test]
    fn resolve_prefers_foreground_child_over_first_child() {
        let tmp = tempfile::tempdir().unwrap();

        let kitty_dir = create_fake_proc_dir(tmp.path(), 1000);
        write_cmdline(&kitty_dir, &["kitty"]);
        write_comm(&kitty_dir, "kitty");
        write_children(&kitty_dir, &[1001]);

        let fish_dir = create_fake_proc_dir(tmp.path(), 1001);
        write_cmdline(&fish_dir, &["fish"]);
        write_comm(&fish_dir, "fish");
        write_children(&fish_dir, &[1002, 1003]);
        // tpgid (field 8) points to 1003 = the foreground process
        fs::write(fish_dir.join("stat"), "1001 (fish) S 0 0 0 0 1003\n").unwrap();

        let htop_dir = create_fake_proc_dir(tmp.path(), 1002);
        write_cmdline(&htop_dir, &["htop"]);
        write_comm(&htop_dir, "htop");

        let btop_dir = create_fake_proc_dir(tmp.path(), 1003);
        write_cmdline(&btop_dir, &["btop"]);
        write_comm(&btop_dir, "btop");
        symlink("/home/user", btop_dir.join("cwd")).unwrap();

        let shell_names = vec!["fish".to_string()];
        let helper_names: Vec<String> = vec![];
        let result = resolve_child_process_at(tmp.path(), 1000, &shell_names, &helper_names, 20);
        assert_eq!(
            result,
            Some((vec!["btop".to_string()], "/home/user".to_string()))
        );
    }

    #[test]
    fn is_shell_detection() {
        let shells = vec!["fish".to_string(), "bash".to_string(), "-fish".to_string()];
        assert!(is_shell("fish", &shells));
        assert!(is_shell("-fish", &shells));
        assert!(!is_shell("btop", &shells));
        assert!(!is_shell("nvim", &shells));
    }

    #[test]
    fn is_helper_detection() {
        let helpers = vec!["kitten".to_string()];
        assert!(is_helper("kitten", &helpers));
        assert!(!is_helper("btop", &helpers));
    }
}
