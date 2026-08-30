#![deny(warnings)]
#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]
mod config;
mod ip;
mod prepare;

use clap::Parser;
use log::{error, info, warn};
use nat_common::{Args, logger};
use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::Path;
use std::process::Command;
use std::thread::sleep;
use std::time::Duration;

const NFTABLES_ETC: &str = "/etc/nftables-nat";
const FILE_NAME_SCRIPT: &str = "/etc/nftables-nat/nat-diy.nft";
const IP_FORWARD: &str = "/proc/sys/net/ipv4/ip_forward";
const IPV6_FORWARD: &str = "/proc/sys/net/ipv6/conf/all/forwarding";
const IPV6_CONF_DIR: &str = "/proc/sys/net/ipv6/conf";
const IPV6_ROUTE_TABLE: &str = "/proc/net/ipv6_route";
const IPV6_INTERFACE_ADDRESSES: &str = "/proc/net/if_inet6";
const CARGO_CRATE_NAME: &str = env!("CARGO_CRATE_NAME");

fn main() -> Result<(), Box<dyn std::error::Error>> {
    logger::init(CARGO_CRATE_NAME);
    // 使用 clap 解析命令行参数
    let args = Args::parse();

    // 启动时解析一次配置文件，并且快速失败
    if let Err(e) = parse_conf(&args).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e)) {
        info!("解析配置文件失败: {e:?}");
        return Err(e.into());
    }
    global_prepare()?;
    Ok(handle_loop(&args)?)
}

fn parse_conf(
    args: &Args,
) -> Result<Vec<config::RuntimeCell>, Box<dyn std::error::Error + Send + Sync>> {
    let nat_cells = if let Some(compatible_config_file) = &args.compatible_config_file {
        config::read_config(compatible_config_file).map_err(|e| {
            info!("读取配置文件失败: {e:?}");
            config::example(compatible_config_file);
            e
        })?
    } else if let Some(toml) = &args.toml {
        config::read_toml_config(toml).map_err(|e| {
            info!("读取配置文件失败: {e:?}");
            if let Err(e) = config::toml_example(toml) {
                info!("{e:?}");
            }
            e
        })?
    } else {
        return Err("请提供配置文件路径".into());
    };
    Ok(nat_cells)
}

fn global_prepare() -> Result<(), io::Error> {
    if let Err(e) = Command::new("/usr/sbin/nft").arg("-v").output() {
        if e.kind() == io::ErrorKind::NotFound {
            let err = "未检测到 nftables，请先安装 nftables (Debian/Ubuntu: apt install nftables, CentOS/RHEL: yum install nftables)";
            error!("{}", err);
            return Err(io::Error::new(io::ErrorKind::NotFound, err));
        }
        return Err(e);
    }

    fs::create_dir_all(NFTABLES_ETC)?;
    // 修改内核参数，开启IPv4端口转发
    match std::fs::write(IP_FORWARD, "1") {
        Ok(_s) => {
            info!("kernel ip_forward config enabled!\n")
        }
        Err(e) => {
            info!(
                "enable ip_forward FAILED! cause: {e:?}\nPlease excute `echo 1 > /proc/sys/net/ipv4/ip_forward` manually\n"
            );
            return Err(e);
        }
    };

    // 开启全局IPv6转发后，accept_ra=1的接口会停止接收RA。
    // 在开启转发前将这些接口提升为2，以保留它们原本接收RA的行为；
    // 显式禁用RA（accept_ra=0）的接口保持不变。
    let ra_interfaces = ipv6_ra_interfaces(
        read_proc_file(Path::new(IPV6_ROUTE_TABLE)),
        read_proc_file(Path::new(IPV6_INTERFACE_ADDRESSES)),
    );
    match preserve_ipv6_router_advertisements(Path::new(IPV6_CONF_DIR), &ra_interfaces) {
        Ok(interfaces) => {
            for interface in interfaces {
                info!("IPv6 RA acceptance preserved on interface {interface} (accept_ra=2)");
            }
        }
        Err(e) => {
            warn!("failed to inspect IPv6 accept_ra settings: {e}");
        }
    }

    // 修改内核参数，开启IPv6端口转发
    match fs::write(IPV6_FORWARD, "1") {
        Ok(_s) => {
            info!("kernel ipv6_forward config enabled!\n")
        }
        Err(e) => {
            info!(
                "enable ipv6_forward FAILED! cause: {e:?}\nPlease excute `echo 1 > /proc/sys/net/ipv6/conf/all/forwarding` manually\n"
            );
            // IPv6转发失败不作为致命错误，因为可能系统不支持IPv6
            info!("IPv6 forwarding setup failed, continuing with IPv4 only...");
        }
    };
    Ok(())
}

fn read_proc_file(path: &Path) -> String {
    match fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(e) => {
            warn!("failed to read {}: {e}", path.display());
            String::new()
        }
    }
}

fn ipv6_ra_interfaces(routes: String, interface_addresses: String) -> HashSet<String> {
    const UNSPECIFIED_ADDRESS: &str = "00000000000000000000000000000000";

    let mut interfaces = HashSet::new();
    for line in routes.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() == 10
            && fields[0] == UNSPECIFIED_ADDRESS
            && fields[1] == "00"
            && fields[9] != "lo"
        {
            interfaces.insert(fields[9].to_string());
        }
    }

    // forwarding可能已经导致真实上联网卡的默认路由过期，或sing-box等程序可能
    // 另外创建了默认路由。因此也纳入仍持有公网IPv6地址的接口，但排除ULA、
    // loopback和容器常见的link-local接口。
    for line in interface_addresses.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() == 6
            && fields[3] == "00"
            && (fields[0].starts_with('2') || fields[0].starts_with('3'))
            && fields[5] != "lo"
        {
            interfaces.insert(fields[5].to_string());
        }
    }

    interfaces
}

fn preserve_ipv6_router_advertisements(
    conf_dir: &Path,
    interfaces: &HashSet<String>,
) -> Result<Vec<String>, io::Error> {
    let mut updated_interfaces = Vec::new();

    for entry in fs::read_dir(conf_dir)? {
        let entry = match entry {
            Ok(entry) => entry,
            Err(e) => {
                warn!("failed to inspect an IPv6 interface sysctl directory: {e}");
                continue;
            }
        };
        let interface = entry.file_name().to_string_lossy().into_owned();
        if !interfaces.contains(&interface) {
            continue;
        }

        let accept_ra_path = entry.path().join("accept_ra");
        let accept_ra = match fs::read_to_string(&accept_ra_path) {
            Ok(value) => value,
            Err(e) => {
                warn!("failed to read {}: {e}", accept_ra_path.display());
                continue;
            }
        };

        if accept_ra.trim() != "1" {
            continue;
        }

        if let Err(e) = fs::write(&accept_ra_path, "2") {
            warn!("failed to write {}: {e}", accept_ra_path.display());
            continue;
        }
        updated_interfaces.push(interface);
    }

    updated_interfaces.sort();
    Ok(updated_interfaces)
}

fn handle_loop(args: &Args) -> Result<(), io::Error> {
    let mut latest_script = String::new();
    loop {
        let nat_cells = match parse_conf(args) {
            Ok(cells) => cells,
            Err(e) => {
                error!("解析配置文件失败: {e:?}");
                if cfg!(debug_assertions) {
                    sleep(Duration::from_secs(5));
                } else {
                    sleep(Duration::new(60, 0));
                }
                continue;
            }
        };
        let script = build_new_script(&nat_cells)?;
        prepare::check_and_prepare()?;
        if script != latest_script {
            info!("当前配置: ");
            for ele in &nat_cells {
                info!("{ele:?}");
            }
            info!("nftables脚本如下：\n{script}");
            latest_script.clone_from(&script);
            let f = File::create(FILE_NAME_SCRIPT);
            if let Ok(mut file) = f {
                file.write_all(script.as_bytes())?;
            }

            let output = Command::new("/usr/sbin/nft")
                .arg("-f")
                .arg(FILE_NAME_SCRIPT)
                .output()?;
            info!(
                "执行/usr/sbin/nft -f {FILE_NAME_SCRIPT} 执行结果: {}",
                output.status
            );
            log::info!("stdout: {}", String::from_utf8_lossy(&output.stdout));
            log::error!("stderr: {}", String::from_utf8_lossy(&output.stderr));
            info!("WAIT:等待配置或目标IP发生改变....\n");
        }

        if cfg!(debug_assertions) {
            sleep(Duration::from_secs(5));
        } else {
            //等待60秒
            sleep(Duration::new(60, 0));
        }
    }
}

fn build_new_script(nat_cells: &[config::RuntimeCell]) -> Result<String, io::Error> {
    //脚本的前缀 - 创建IPv4和IPv6表
    let mut script = String::from(
        "#!/usr/sbin/nft -f\n\
        \n\
        # IPv4 NAT table\n\
        add table ip self-nat\n\
        delete table ip self-nat\n\
        add table ip self-nat\n\
        add chain ip self-nat PREROUTING { type nat hook prerouting priority -110 ; }\n\
        add chain ip self-nat POSTROUTING { type nat hook postrouting priority 110 ; }\n\
        \n\
        # IPv6 NAT table\n\
        add table ip6 self-nat\n\
        delete table ip6 self-nat\n\
        add table ip6 self-nat\n\
        add chain ip6 self-nat PREROUTING { type nat hook prerouting priority -110 ; }\n\
        add chain ip6 self-nat POSTROUTING { type nat hook postrouting priority 110 ; }\n\
        \n\
        # IPv4 Drop table\n\
        add table ip self-filter\n\
        delete table ip self-filter\n\
        add table ip self-filter\n\
        add chain ip self-filter INPUT { type filter hook input priority filter - 1 ; }\n\
        add chain ip self-filter FORWARD { type filter hook forward priority filter - 1 ; }\n\
        \n\
        # IPv6 Drop table\n\
        add table ip6 self-filter\n\
        delete table ip6 self-filter\n\
        add table ip6 self-filter\n\
        add chain ip6 self-filter INPUT { type filter hook input priority filter - 1 ; }\n\
        add chain ip6 self-filter FORWARD { type filter hook forward priority filter - 1 ; }\n\
        ",
    );

    for x in nat_cells.iter() {
        match x.build() {
            Ok(rule) => script += &rule,
            Err(e) => {
                log::error!("Failed to build rule for {x:?}: {e}");
            }
        }
    }
    Ok(script)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn preserve_ra_only_updates_enabled_interfaces() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let conf_dir = std::env::temp_dir().join(format!(
            "nftables-nat-rust-accept-ra-{}-{unique}",
            std::process::id()
        ));

        for interface in ["all", "default", "eth0", "eth1"] {
            fs::create_dir_all(conf_dir.join(interface)).unwrap();
        }
        fs::write(conf_dir.join("all/accept_ra"), "1\n").unwrap();
        fs::write(conf_dir.join("default/accept_ra"), "1\n").unwrap();
        fs::write(conf_dir.join("eth0/accept_ra"), "1\n").unwrap();
        fs::write(conf_dir.join("eth1/accept_ra"), "0\n").unwrap();

        let interfaces = HashSet::from(["eth0".to_string(), "eth1".to_string()]);
        let updated = preserve_ipv6_router_advertisements(&conf_dir, &interfaces).unwrap();

        assert_eq!(updated, vec!["eth0"]);
        assert_eq!(
            fs::read_to_string(conf_dir.join("all/accept_ra")).unwrap(),
            "1\n"
        );
        assert_eq!(
            fs::read_to_string(conf_dir.join("default/accept_ra")).unwrap(),
            "1\n"
        );
        assert_eq!(
            fs::read_to_string(conf_dir.join("eth0/accept_ra")).unwrap(),
            "2"
        );
        assert_eq!(
            fs::read_to_string(conf_dir.join("eth1/accept_ra")).unwrap(),
            "0\n"
        );

        fs::remove_dir_all(conf_dir).unwrap();
    }

    #[test]
    fn ra_interfaces_include_default_routes_and_public_addresses() {
        let routes = concat!(
            "00000000000000000000000000000000 00 00000000000000000000000000000000 00 ",
            "fe800000000000000000000000000001 00000064 00000000 00000000 00000003 eth0\n"
        );
        let addresses = "26000000000000000000000000000001 03 40 00 00 eth1\n".to_string();

        let interfaces = ipv6_ra_interfaces(routes.to_string(), addresses);

        assert_eq!(
            interfaces,
            HashSet::from(["eth0".to_string(), "eth1".to_string()])
        );
    }

    #[test]
    fn ra_interfaces_fall_back_to_public_ipv6_addresses() {
        let addresses = concat!(
            "26000000000000000000000000000001 03 40 00 00 eth0\n",
            "fd000000000000000000000000000001 04 40 00 00 wg0\n",
            "fe800000000000000000000000000001 05 40 20 80 veth0\n"
        );

        let interfaces = ipv6_ra_interfaces(String::new(), addresses.to_string());

        assert_eq!(interfaces, HashSet::from(["eth0".to_string()]));
    }
}
