use std::{collections::HashMap, path::Path, process::Output, sync::Arc};

use serde::{Deserialize, Deserializer, Serialize};
use smol::process::Command;

use crate::{DevContainerConfig, devcontainer_api::DevContainerUp};

#[derive(Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
struct DevContainer {
    image: Option<String>,
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum RenameMeError {
    DevContainerParseFailed,
    UnmappedError,
}

fn deserialize_metadata<'de, D>(
    deserializer: D,
) -> Result<Option<Vec<HashMap<String, serde_json_lenient::Value>>>, D::Error>
where
    D: Deserializer<'de>,
{
    let s: Option<String> = Option::deserialize(deserializer)?;
    match s {
        Some(json_string) => {
            dbg!(&json_string);
            let parsed: Vec<HashMap<String, serde_json_lenient::Value>> =
                serde_json_lenient::from_str(&json_string).map_err(serde::de::Error::custom)?;
            Ok(Some(parsed))
        }
        None => Ok(None),
    }
}

#[derive(Debug, Deserialize, Serialize, Eq, PartialEq)]
struct DockerConfigLabels {
    #[serde(
        rename = "devcontainer.metadata",
        deserialize_with = "deserialize_metadata"
    )]
    metadata: Option<Vec<HashMap<String, serde_json_lenient::Value>>>,
}

#[derive(Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(rename_all = "PascalCase")]
struct DockerInspectConfig {
    labels: DockerConfigLabels,
}

#[derive(Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(rename_all = "PascalCase")]
struct DockerInspectMount {
    source: String,
    destination: String,
}

#[derive(Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(rename_all = "PascalCase")]
struct DockerInspect {
    config: DockerInspectConfig,
    mounts: Option<Vec<DockerInspectMount>>,
}

#[derive(Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(rename_all = "PascalCase")]
struct DockerPs {
    #[serde(rename = "ID")]
    id: String,
}

// TODO podman
fn docker_cli() -> &'static str {
    "docker"
}

pub(crate) async fn spawn_dev_container(
    config: DevContainerConfig,
    local_project_path: Arc<&Path>,
) -> Result<DevContainerUp, RenameMeError> {
    log::info!(
        "Starting dev container for project path {}",
        &local_project_path.display()
    );

    let labels = vec![
        (
            "devcontainer.local_folder",
            (&local_project_path.display()).to_string(),
        ),
        (
            "devcontainer.config_file",
            config.config_path.display().to_string(),
        ),
    ];

    let config_path = local_project_path.join(config.config_path);

    let devcontainer_contents = std::fs::read_to_string(&config_path).map_err(|e| {
        log::error!("Unable to read devcontainer contents: {e}");
        RenameMeError::UnmappedError
    })?;

    let devcontainer = deserialize_devcontainer_json(&devcontainer_contents)?;

    let mut command = create_docker_query_containers(Some(&labels))?;

    let output = command.output().await.map_err(|e| {
        log::error!("Error executing docker query containers command: {e}");
        RenameMeError::UnmappedError
    })?;

    // Execute command, get back ids (or not)
    let docker_ps: Option<DockerPs> = deserialize_json_output(output)?;

    if docker_ps.is_none() {
        log::info!("no docker image found for query, creating one");
        let mut docker_run_command =
            create_docker_run_command(&devcontainer, &local_project_path, Some(&labels))?;

        if let Err(e) = docker_run_command.output().await {
            log::error!("Error running docker run: {e}");
        }
    }

    log::info!("Trying to get docker container ID again, now that we've done docker run");
    let mut command = create_docker_query_containers(Some(&labels))?;

    let output = command.output().await.map_err(|e| {
        log::error!("Error executing docker query containers: {e}");
        RenameMeError::UnmappedError
    })?;

    let docker_ps: Option<DockerPs> = deserialize_json_output(output)?;

    let Some(docker_ps) = docker_ps else {
        log::error!("After creating with docker run, we still couldn't find anything");
        return Err(RenameMeError::UnmappedError);
    };

    log::info!("Getting labels for container {}", &docker_ps.id);
    let mut command = create_docker_inspect(&docker_ps.id)?;

    let output = command.output().await.map_err(|e| {
        log::error!(
            "Error getting labels from docker image {}: {e}",
            &docker_ps.id
        );
        RenameMeError::UnmappedError
    })?;

    let Some(docker_inspect): Option<DockerInspect> = deserialize_json_output(output)? else {
        log::error!("Could not deserialize docker labels");
        return Err(RenameMeError::UnmappedError);
    };

    let remote_user = get_remote_user_from_config(&docker_inspect)?;

    let remote_folder =
        get_remote_dir_from_config(&docker_inspect, (&local_project_path.display()).to_string())?;

    Ok(DevContainerUp {
        _outcome: "todo".to_string(),
        container_id: docker_ps.id,
        remote_user: remote_user,
        remote_workspace_folder: remote_folder,
    })
}

// For this to work, I have to ignore quiet and instead do format=json
fn deserialize_json_output<T>(output: Output) -> Result<Option<T>, RenameMeError>
where
    T: for<'de> Deserialize<'de>,
{
    if output.status.success() {
        let raw = String::from_utf8_lossy(&output.stdout);
        if raw.is_empty() {
            return Ok(None);
        }
        serde_json_lenient::from_str(&raw).map_err(|e| {
            dbg!(&e);
            RenameMeError::UnmappedError
        })
    } else {
        Err(RenameMeError::UnmappedError)
    }
}

fn deserialize_devcontainer_json(json: &str) -> Result<DevContainer, RenameMeError> {
    match serde_json_lenient::from_str(json) {
        Ok(devcontainer) => Ok(devcontainer),
        Err(e) => {
            dbg!(&e);
            Err(RenameMeError::DevContainerParseFailed)
        }
    }
}

fn create_docker_inspect(id: &str) -> Result<Command, RenameMeError> {
    let mut command = smol::process::Command::new(docker_cli());
    command.args(&["inspect", "--format={{json . }}", id]);
    Ok(command)
}

fn create_docker_query_containers(
    filter_labels: Option<&Vec<(&str, String)>>,
) -> Result<Command, RenameMeError> {
    let mut command = smol::process::Command::new(docker_cli());
    command.args(&["ps", "-a"]);

    if let Some(labels) = filter_labels {
        for (key, value) in labels {
            command.arg("--filter");
            command.arg(format!("label={key}={value}"));
        }
    }
    command.arg("--format=json");
    Ok(command)
}

fn create_docker_run_command(
    devcontainer: &DevContainer,
    local_project_directory: &Arc<&Path>,
    labels: Option<&Vec<(&str, String)>>,
) -> Result<Command, RenameMeError> {
    let Some(image) = &devcontainer.image else {
        return Err(RenameMeError::UnmappedError);
    };
    // let remote_user = get_remote_user_from_config(config)?;

    let Some(project_directory) = local_project_directory.file_name() else {
        return Err(RenameMeError::UnmappedError);
    };
    let remote_workspace_folder = format!("/workspaces/{}", project_directory.display()); // TODO workspaces should be overridable

    let mut command = Command::new(docker_cli());

    // TODO TODO
    command.arg("run");
    command.arg("--sig-proxy=false");
    command.arg("-d");
    // command.arg("-a");
    // command.arg("STDOUT");
    // command.arg("-a");
    // command.arg("STDERR");
    command.arg("--mount");
    command.arg(format!(
        "type=bind,source={},target={},consistency=cached",
        local_project_directory.display(),
        remote_workspace_folder
    ));

    if let Some(labels) = labels {
        for (key, val) in labels {
            command.arg("-l");
            command.arg(format!("{}={}", key, val));
        }
    }

    command.arg("--entrypoint");
    command.arg("/bin/sh");
    command.arg(image);
    command.arg("-c");
    command.arg(
        "
echo Container started
trap \"exit 0\" 15
exec \"$@\"
while sleep 1 & wait $!; do :; done
        "
        .trim(),
    );
    command.arg("-");

    Ok(command)
}

fn get_remote_dir_from_config(
    config: &DockerInspect,
    local_dir: String,
) -> Result<String, RenameMeError> {
    let Some(mounts) = &config.mounts else {
        return Err(RenameMeError::UnmappedError);
    };
    for mount in mounts {
        if mount.source == local_dir {
            return Ok(mount.destination.clone());
        }
    }
    Err(RenameMeError::UnmappedError)
}

fn get_remote_user_from_config(config: &DockerInspect) -> Result<String, RenameMeError> {
    let Some(metadata) = &config.config.labels.metadata else {
        return Err(RenameMeError::UnmappedError);
    };
    for metadatum in metadata {
        if let Some(remote_user) = metadatum.get("remoteUser") {
            if let Some(remote_user_str) = remote_user.as_str() {
                return Ok(remote_user_str.to_string());
            }
        }
    }
    Err(RenameMeError::UnmappedError)
}

#[cfg(test)]
mod test {
    use std::{
        collections::HashMap,
        ffi::OsStr,
        path::Path,
        process::{ExitStatus, Output},
        sync::Arc,
    };

    use crate::model::{
        DevContainer, DockerConfigLabels, DockerInspect, DockerInspectConfig, DockerPs,
        RenameMeError, create_docker_inspect, create_docker_run_command,
        deserialize_devcontainer_json, deserialize_json_output, get_remote_dir_from_config,
        get_remote_user_from_config,
    };

    #[test]
    fn should_deserialize_simple_devcontainer_json() {
        let given_bad_json = "{ \"image\": 123 }";

        let result: Result<DevContainer, RenameMeError> =
            deserialize_devcontainer_json(given_bad_json);

        assert!(result.is_err());
        assert_eq!(
            result.expect_err("err"),
            RenameMeError::DevContainerParseFailed
        );

        let given_good_json = "{\"image\": \"mcr.microsoft.com/devcontainers/base:ubuntu\"}";

        let result: Result<DevContainer, RenameMeError> =
            deserialize_devcontainer_json(given_good_json);

        assert!(result.is_ok());
        assert_eq!(
            result.expect("ok"),
            DevContainer {
                image: Some(String::from("mcr.microsoft.com/devcontainers/base:ubuntu"))
            }
        );
    }

    #[test]
    fn should_get_remote_user_from_devcontainer_config() {
        let mut metadata = HashMap::new();
        metadata.insert(
            "remoteUser".to_string(),
            serde_json_lenient::Value::String("vsCode".to_string()),
        );
        let given_docker_config = DockerInspect {
            config: DockerInspectConfig {
                labels: DockerConfigLabels {
                    metadata: Some(vec![metadata]),
                },
            },
            mounts: None,
        };

        let remote_user = get_remote_user_from_config(&given_docker_config);

        assert!(remote_user.is_ok());
        let remote_user = remote_user.expect("ok");
        assert_eq!(&remote_user, "vsCode")
    }

    #[test]
    fn should_create_correct_docker_run_command() {
        let mut metadata = HashMap::new();
        metadata.insert(
            "remoteUser".to_string(),
            serde_json_lenient::Value::String("vsCode".to_string()),
        );
        let given_devcontainer = DevContainer {
            image: Some("mcr.microsoft.com/devcontainers/base:ubuntu".to_string()),
        };

        let labels = vec![
            ("label1", "value1".to_string()),
            ("label2", "value2".to_string()),
        ];

        let docker_run_command = create_docker_run_command(
            &given_devcontainer,
            &Arc::new(Path::new("/local/project_app")),
            Some(&labels),
        );

        assert!(docker_run_command.is_ok());
        let docker_run_command = docker_run_command.expect("ok");

        assert_eq!(docker_run_command.get_program(), "docker");
        assert_eq!(
            docker_run_command.get_args().collect::<Vec<&OsStr>>(),
            vec![
                OsStr::new("run"),
                OsStr::new("--sig-proxy=false"),
                OsStr::new("-d"),
                OsStr::new("--mount"),
                OsStr::new(
                    "type=bind,source=/local/project_app,target=/workspaces/project_app,consistency=cached"
                ),
                OsStr::new("-l"),
                OsStr::new("label1=value1"),
                OsStr::new("-l"),
                OsStr::new("label2=value2"),
                OsStr::new("--entrypoint"),
                OsStr::new("/bin/sh"),
                OsStr::new("mcr.microsoft.com/devcontainers/base:ubuntu"),
                OsStr::new("-c"),
                OsStr::new(
                    "
echo Container started
trap \"exit 0\" 15
exec \"$@\"
while sleep 1 & wait $!; do :; done
                    "
                    .trim()
                ),
                OsStr::new("-"),
            ]
        )
    }

    #[test]
    fn should_deserialize_docker_ps_with_filters() {
        // First, deserializes empty
        let empty_output = Output {
            status: ExitStatus::default(),
            stderr: vec![],
            stdout: String::from("").into_bytes(),
        };

        let result: Option<DockerPs> = deserialize_json_output(empty_output).unwrap();

        assert!(result.is_none());

        let full_output = Output {
            status: ExitStatus::default(),
            stderr: vec![],
            stdout: String::from(r#"
{
    "Command": "\"/bin/sh -c 'echo Co…\"",
    "CreatedAt": "2026-02-04 15:44:21 -0800 PST",
    "ID": "abdb6ab59573",
    "Image": "mcr.microsoft.com/devcontainers/base:ubuntu",
    "Labels": "desktop.docker.io/mounts/0/Source=/somepath/cli,desktop.docker.io/mounts/0/SourceKind=hostFile,desktop.docker.io/mounts/0/Target=/workspaces/cli,desktop.docker.io/ports.scheme=v2,dev.containers.features=common,dev.containers.id=base-ubuntu,dev.containers.release=v0.4.24,dev.containers.source=https://github.com/devcontainers/images,dev.containers.timestamp=Fri, 30 Jan 2026 16:52:34 GMT,dev.containers.variant=noble,devcontainer.config_file=/somepath/cli/.devcontainer/dev_container_2/devcontainer.json,devcontainer.local_folder=/somepath/cli,devcontainer.metadata=[{\"id\":\"ghcr.io/devcontainers/features/common-utils:2\"},{\"id\":\"ghcr.io/devcontainers/features/git:1\",\"customizations\":{\"vscode\":{\"settings\":{\"github.copilot.chat.codeGeneration.instructions\":[{\"text\":\"This dev container includes an up-to-date version of Git, built from source as needed, pre-installed and available on the `PATH`.\"}]}}}},{\"remoteUser\":\"vscode\"}],org.opencontainers.image.ref.name=ubuntu,org.opencontainers.image.version=24.04,version=2.1.6",
    "LocalVolumes": "0",
    "Mounts": "/host_mnt/User…",
    "Names": "objective_haslett",
    "Networks": "bridge",
    "Platform": {
    "architecture": "arm64",
    "os": "linux"
    },
    "Ports": "",
    "RunningFor": "47 hours ago",
    "Size": "0B",
    "State": "running",
    "Status": "Up 47 hours"
}
                "#).into_bytes(),
        };

        let result: Option<DockerPs> = deserialize_json_output(full_output).unwrap();

        assert!(result.is_some());
        let result = result.unwrap();
        assert_eq!(result.id, "abdb6ab59573".to_string());
    }

    #[test]
    fn should_create_docker_inspect_command() {
        let given_id = "given_docker_id";

        let command = create_docker_inspect(given_id);

        assert!(command.is_ok());
        let command = command.unwrap();

        assert_eq!(
            command.get_args().collect::<Vec<&OsStr>>(),
            vec![
                OsStr::new("inspect"),
                OsStr::new("--format={{json . }}"),
                OsStr::new(given_id)
            ]
        )
    }

    #[test]
    fn should_deserialize_docker_labels() {
        let given_config = r#"
{"Id":"fca38334e88f9045a8cc41ebe0dc94e955a74dda2e526ed7546cf7a0f27b5b75","Created":"2026-02-09T23:22:15.585555798Z","Path":"/bin/sh","Args":["-c","echo Container started\ntrap \"exit 0\" 15\nexec \"$@\"\nwhile sleep 1 & wait $!; do :; done","-"],"State":{"Status":"running","Running":true,"Paused":false,"Restarting":false,"OOMKilled":false,"Dead":false,"Pid":94196,"ExitCode":0,"Error":"","StartedAt":"2026-02-09T23:22:15.628810548Z","FinishedAt":"0001-01-01T00:00:00Z"},"Image":"sha256:3dcb059253b2ebb44de3936620e1cff3dadcd2c1c982d579081ca8128c1eb319","ResolvConfPath":"/var/lib/docker/containers/fca38334e88f9045a8cc41ebe0dc94e955a74dda2e526ed7546cf7a0f27b5b75/resolv.conf","HostnamePath":"/var/lib/docker/containers/fca38334e88f9045a8cc41ebe0dc94e955a74dda2e526ed7546cf7a0f27b5b75/hostname","HostsPath":"/var/lib/docker/containers/fca38334e88f9045a8cc41ebe0dc94e955a74dda2e526ed7546cf7a0f27b5b75/hosts","LogPath":"/var/lib/docker/containers/fca38334e88f9045a8cc41ebe0dc94e955a74dda2e526ed7546cf7a0f27b5b75/fca38334e88f9045a8cc41ebe0dc94e955a74dda2e526ed7546cf7a0f27b5b75-json.log","Name":"/magical_easley","RestartCount":0,"Driver":"overlayfs","Platform":"linux","MountLabel":"","ProcessLabel":"","AppArmorProfile":"","ExecIDs":null,"HostConfig":{"Binds":null,"ContainerIDFile":"","LogConfig":{"Type":"json-file","Config":{}},"NetworkMode":"bridge","PortBindings":{},"RestartPolicy":{"Name":"no","MaximumRetryCount":0},"AutoRemove":false,"VolumeDriver":"","VolumesFrom":null,"ConsoleSize":[0,0],"CapAdd":null,"CapDrop":null,"CgroupnsMode":"private","Dns":[],"DnsOptions":[],"DnsSearch":[],"ExtraHosts":null,"GroupAdd":null,"IpcMode":"private","Cgroup":"","Links":null,"OomScoreAdj":0,"PidMode":"","Privileged":false,"PublishAllPorts":false,"ReadonlyRootfs":false,"SecurityOpt":null,"UTSMode":"","UsernsMode":"","ShmSize":67108864,"Runtime":"runc","Isolation":"","CpuShares":0,"Memory":0,"NanoCpus":0,"CgroupParent":"","BlkioWeight":0,"BlkioWeightDevice":[],"BlkioDeviceReadBps":[],"BlkioDeviceWriteBps":[],"BlkioDeviceReadIOps":[],"BlkioDeviceWriteIOps":[],"CpuPeriod":0,"CpuQuota":0,"CpuRealtimePeriod":0,"CpuRealtimeRuntime":0,"CpusetCpus":"","CpusetMems":"","Devices":[],"DeviceCgroupRules":null,"DeviceRequests":null,"MemoryReservation":0,"MemorySwap":0,"MemorySwappiness":null,"OomKillDisable":null,"PidsLimit":null,"Ulimits":[],"CpuCount":0,"CpuPercent":0,"IOMaximumIOps":0,"IOMaximumBandwidth":0,"Mounts":[{"Type":"bind","Source":"/somepath/rustwebstarter","Target":"/workspaces/rustwebstarter","Consistency":"cached"}],"MaskedPaths":["/proc/asound","/proc/acpi","/proc/interrupts","/proc/kcore","/proc/keys","/proc/latency_stats","/proc/timer_list","/proc/timer_stats","/proc/sched_debug","/proc/scsi","/sys/firmware","/sys/devices/virtual/powercap"],"ReadonlyPaths":["/proc/bus","/proc/fs","/proc/irq","/proc/sys","/proc/sysrq-trigger"]},"GraphDriver":{"Data":null,"Name":"overlayfs"},"Mounts":[{"Type":"bind","Source":"/somepath/rustwebstarter","Destination":"/workspaces/rustwebstarter","Mode":"","RW":true,"Propagation":"rprivate"}],"Config":{"Hostname":"fca38334e88f","Domainname":"","User":"root","AttachStdin":false,"AttachStdout":false,"AttachStderr":false,"Tty":false,"OpenStdin":false,"StdinOnce":false,"Env":["PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"],"Cmd":["-c","echo Container started\ntrap \"exit 0\" 15\nexec \"$@\"\nwhile sleep 1 & wait $!; do :; done","-"],"Image":"mcr.microsoft.com/devcontainers/base:ubuntu","Volumes":null,"WorkingDir":"","Entrypoint":["/bin/sh"],"OnBuild":null,"Labels":{"dev.containers.features":"common","dev.containers.id":"base-ubuntu","dev.containers.release":"v0.4.24","dev.containers.source":"https://github.com/devcontainers/images","dev.containers.timestamp":"Fri, 30 Jan 2026 16:52:34 GMT","dev.containers.variant":"noble","devcontainer.config_file":".devcontainer/devcontainer.json","devcontainer.local_folder":"/somepath/rustwebstarter","devcontainer.metadata":"[ {\"id\":\"ghcr.io/devcontainers/features/common-utils:2\"}, {\"id\":\"ghcr.io/devcontainers/features/git:1\",\"customizations\":{\"vscode\":{\"settings\":{\"github.copilot.chat.codeGeneration.instructions\":[{\"text\":\"This dev container includes an up-to-date version of Git, built from source as needed, pre-installed and available on the `PATH`.\"}]}}}}, {\"remoteUser\":\"vscode\"} ]","org.opencontainers.image.ref.name":"ubuntu","org.opencontainers.image.version":"24.04","version":"2.1.6"},"StopTimeout":1},"NetworkSettings":{"Bridge":"","SandboxID":"ef2f9f610d87de6bf6061627a0cadb2b89e918bafba92e0e4e9e877d092315c7","SandboxKey":"/var/run/docker/netns/ef2f9f610d87","Ports":{},"HairpinMode":false,"LinkLocalIPv6Address":"","LinkLocalIPv6PrefixLen":0,"SecondaryIPAddresses":null,"SecondaryIPv6Addresses":null,"EndpointID":"50b3501ee308c36e212a025b4f4ddd4ffbd6aeeafa986350ea7d9fe5e16e2c8c","Gateway":"172.17.0.1","GlobalIPv6Address":"","GlobalIPv6PrefixLen":0,"IPAddress":"172.17.0.4","IPPrefixLen":16,"IPv6Gateway":"","MacAddress":"ca:02:9f:22:fd:8e","Networks":{"bridge":{"IPAMConfig":null,"Links":null,"Aliases":null,"MacAddress":"ca:02:9f:22:fd:8e","DriverOpts":null,"GwPriority":0,"NetworkID":"51bb8ccc4d1281db44f16d915963fc728619d4a68e2f90e5ea8f1cb94885063e","EndpointID":"50b3501ee308c36e212a025b4f4ddd4ffbd6aeeafa986350ea7d9fe5e16e2c8c","Gateway":"172.17.0.1","IPAddress":"172.17.0.4","IPPrefixLen":16,"IPv6Gateway":"","GlobalIPv6Address":"","GlobalIPv6PrefixLen":0,"DNSNames":null}}},"ImageManifestDescriptor":{"mediaType":"application/vnd.oci.image.manifest.v1+json","digest":"sha256:39c3436527190561948236894c55b59fa58aa08d68d8867e703c8d5ab72a3593","size":2195,"platform":{"architecture":"arm64","os":"linux"}}}
            "#;

        let deserialized = serde_json_lenient::from_str::<DockerInspect>(given_config);
        // assert!(deserialized.is_ok());
        let config = deserialized.unwrap();
        let remote_user = get_remote_user_from_config(&config);

        assert!(remote_user.is_ok());
        assert_eq!(remote_user.unwrap(), "vscode".to_string())
    }

    #[test]
    fn should_get_target_dir_from_docker_inspect() {
        let given_config = r#"
{
  "Id": "abdb6ab59573659b11dac9f4973796741be35b642c9b48960709304ce46dbf85",
  "Created": "2026-02-04T23:44:21.802688084Z",
  "Path": "/bin/sh",
  "Args": [
    "-c",
    "echo Container started\ntrap \"exit 0\" 15\n\nexec \"$@\"\nwhile sleep 1 & wait $!; do :; done",
    "-"
  ],
  "State": {
    "Status": "running",
    "Running": true,
    "Paused": false,
    "Restarting": false,
    "OOMKilled": false,
    "Dead": false,
    "Pid": 23087,
    "ExitCode": 0,
    "Error": "",
    "StartedAt": "2026-02-04T23:44:21.954875084Z",
    "FinishedAt": "0001-01-01T00:00:00Z"
  },
  "Image": "sha256:3dcb059253b2ebb44de3936620e1cff3dadcd2c1c982d579081ca8128c1eb319",
  "ResolvConfPath": "/var/lib/docker/containers/abdb6ab59573659b11dac9f4973796741be35b642c9b48960709304ce46dbf85/resolv.conf",
  "HostnamePath": "/var/lib/docker/containers/abdb6ab59573659b11dac9f4973796741be35b642c9b48960709304ce46dbf85/hostname",
  "HostsPath": "/var/lib/docker/containers/abdb6ab59573659b11dac9f4973796741be35b642c9b48960709304ce46dbf85/hosts",
  "LogPath": "/var/lib/docker/containers/abdb6ab59573659b11dac9f4973796741be35b642c9b48960709304ce46dbf85/abdb6ab59573659b11dac9f4973796741be35b642c9b48960709304ce46dbf85-json.log",
  "Name": "/objective_haslett",
  "RestartCount": 0,
  "Driver": "overlayfs",
  "Platform": "linux",
  "MountLabel": "",
  "ProcessLabel": "",
  "AppArmorProfile": "",
  "ExecIDs": [
    "008019d93df4107fcbba78bcc6e1ed7e121844f36c26aca1a56284655a6adb53"
  ],
  "HostConfig": {
    "Binds": null,
    "ContainerIDFile": "",
    "LogConfig": {
      "Type": "json-file",
      "Config": {}
    },
    "NetworkMode": "bridge",
    "PortBindings": {},
    "RestartPolicy": {
      "Name": "no",
      "MaximumRetryCount": 0
    },
    "AutoRemove": false,
    "VolumeDriver": "",
    "VolumesFrom": null,
    "ConsoleSize": [
      0,
      0
    ],
    "CapAdd": null,
    "CapDrop": null,
    "CgroupnsMode": "private",
    "Dns": [],
    "DnsOptions": [],
    "DnsSearch": [],
    "ExtraHosts": null,
    "GroupAdd": null,
    "IpcMode": "private",
    "Cgroup": "",
    "Links": null,
    "OomScoreAdj": 0,
    "PidMode": "",
    "Privileged": false,
    "PublishAllPorts": false,
    "ReadonlyRootfs": false,
    "SecurityOpt": null,
    "UTSMode": "",
    "UsernsMode": "",
    "ShmSize": 67108864,
    "Runtime": "runc",
    "Isolation": "",
    "CpuShares": 0,
    "Memory": 0,
    "NanoCpus": 0,
    "CgroupParent": "",
    "BlkioWeight": 0,
    "BlkioWeightDevice": [],
    "BlkioDeviceReadBps": [],
    "BlkioDeviceWriteBps": [],
    "BlkioDeviceReadIOps": [],
    "BlkioDeviceWriteIOps": [],
    "CpuPeriod": 0,
    "CpuQuota": 0,
    "CpuRealtimePeriod": 0,
    "CpuRealtimeRuntime": 0,
    "CpusetCpus": "",
    "CpusetMems": "",
    "Devices": [],
    "DeviceCgroupRules": null,
    "DeviceRequests": null,
    "MemoryReservation": 0,
    "MemorySwap": 0,
    "MemorySwappiness": null,
    "OomKillDisable": null,
    "PidsLimit": null,
    "Ulimits": [],
    "CpuCount": 0,
    "CpuPercent": 0,
    "IOMaximumIOps": 0,
    "IOMaximumBandwidth": 0,
    "Mounts": [
      {
        "Type": "bind",
        "Source": "/somepath/cli",
        "Target": "/workspaces/cli",
        "Consistency": "cached"
      }
    ],
    "MaskedPaths": [
      "/proc/asound",
      "/proc/acpi",
      "/proc/interrupts",
      "/proc/kcore",
      "/proc/keys",
      "/proc/latency_stats",
      "/proc/timer_list",
      "/proc/timer_stats",
      "/proc/sched_debug",
      "/proc/scsi",
      "/sys/firmware",
      "/sys/devices/virtual/powercap"
    ],
    "ReadonlyPaths": [
      "/proc/bus",
      "/proc/fs",
      "/proc/irq",
      "/proc/sys",
      "/proc/sysrq-trigger"
    ]
  },
  "GraphDriver": {
    "Data": null,
    "Name": "overlayfs"
  },
  "Mounts": [
    {
      "Type": "bind",
      "Source": "/somepath/cli",
      "Destination": "/workspaces/cli",
      "Mode": "",
      "RW": true,
      "Propagation": "rprivate"
    }
  ],
  "Config": {
    "Hostname": "abdb6ab59573",
    "Domainname": "",
    "User": "root",
    "AttachStdin": false,
    "AttachStdout": true,
    "AttachStderr": true,
    "Tty": false,
    "OpenStdin": false,
    "StdinOnce": false,
    "Env": [
      "PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"
    ],
    "Cmd": [
      "-c",
      "echo Container started\ntrap \"exit 0\" 15\n\nexec \"$@\"\nwhile sleep 1 & wait $!; do :; done",
      "-"
    ],
    "Image": "mcr.microsoft.com/devcontainers/base:ubuntu",
    "Volumes": null,
    "WorkingDir": "",
    "Entrypoint": [
      "/bin/sh"
    ],
    "OnBuild": null,
    "Labels": {
      "dev.containers.features": "common",
      "dev.containers.id": "base-ubuntu",
      "dev.containers.release": "v0.4.24",
      "dev.containers.source": "https://github.com/devcontainers/images",
      "dev.containers.timestamp": "Fri, 30 Jan 2026 16:52:34 GMT",
      "dev.containers.variant": "noble",
      "devcontainer.config_file": "/somepath/cli/.devcontainer/dev_container_2/devcontainer.json",
      "devcontainer.local_folder": "/somepath/cli",
      "devcontainer.metadata": "[{\"id\":\"ghcr.io/devcontainers/features/common-utils:2\"},{\"id\":\"ghcr.io/devcontainers/features/git:1\",\"customizations\":{\"vscode\":{\"settings\":{\"github.copilot.chat.codeGeneration.instructions\":[{\"text\":\"This dev container includes an up-to-date version of Git, built from source as needed, pre-installed and available on the `PATH`.\"}]}}}},{\"remoteUser\":\"vscode\"}]",
      "org.opencontainers.image.ref.name": "ubuntu",
      "org.opencontainers.image.version": "24.04",
      "version": "2.1.6"
    },
    "StopTimeout": 1
  },
  "NetworkSettings": {
    "Bridge": "",
    "SandboxID": "2a94990d542fe532deb75f1cc67f761df2d669e3b41161f914079e88516cc54b",
    "SandboxKey": "/var/run/docker/netns/2a94990d542f",
    "Ports": {},
    "HairpinMode": false,
    "LinkLocalIPv6Address": "",
    "LinkLocalIPv6PrefixLen": 0,
    "SecondaryIPAddresses": null,
    "SecondaryIPv6Addresses": null,
    "EndpointID": "ef5b35a8fbb145565853e1a1d960e737fcc18c20920e96494e4c0cfc55683570",
    "Gateway": "172.17.0.1",
    "GlobalIPv6Address": "",
    "GlobalIPv6PrefixLen": 0,
    "IPAddress": "172.17.0.3",
    "IPPrefixLen": 16,
    "IPv6Gateway": "",
    "MacAddress": "",
    "Networks": {
      "bridge": {
        "IPAMConfig": null,
        "Links": null,
        "Aliases": null,
        "MacAddress": "9a:ec:af:8a:ac:81",
        "DriverOpts": null,
        "GwPriority": 0,
        "NetworkID": "51bb8ccc4d1281db44f16d915963fc728619d4a68e2f90e5ea8f1cb94885063e",
        "EndpointID": "ef5b35a8fbb145565853e1a1d960e737fcc18c20920e96494e4c0cfc55683570",
        "Gateway": "172.17.0.1",
        "IPAddress": "172.17.0.3",
        "IPPrefixLen": 16,
        "IPv6Gateway": "",
        "GlobalIPv6Address": "",
        "GlobalIPv6PrefixLen": 0,
        "DNSNames": null
      }
    }
  },
  "ImageManifestDescriptor": {
    "mediaType": "application/vnd.oci.image.manifest.v1+json",
    "digest": "sha256:39c3436527190561948236894c55b59fa58aa08d68d8867e703c8d5ab72a3593",
    "size": 2195,
    "platform": {
      "architecture": "arm64",
      "os": "linux"
    }
  }
}
            "#;
        let config = serde_json_lenient::from_str::<DockerInspect>(given_config).unwrap();

        let target_dir = get_remote_dir_from_config(&config, "/somepath/cli".to_string());

        assert!(target_dir.is_ok());
        assert_eq!(target_dir.unwrap(), "/workspaces/cli".to_string());
    }

    // Next, create relevant docker command
    //
    // Next, create appropriate response to user
}
