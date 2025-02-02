use std::collections::HashMap;
use std::fs::File;
use std::io::Read;
use std::sync::Arc;
use std::time::{
    SystemTime,
    UNIX_EPOCH,
};

use futures::stream::{
    self,
    FuturesUnordered,
    StreamExt,
};
use hostsfile::HostsBuilder;
use k8s_openapi::api::core::v1::Pod;
use kftray_commons::config::get_config;
use kftray_commons::config_state::get_configs_state;
use kftray_commons::models::{
    config_model::Config,
    config_state_model::ConfigState,
    response::CustomResponse,
};
use kftray_commons::utils::config_dir::get_pod_manifest_path;
use kftray_commons::utils::config_state::update_config_state;
use kube::api::{
    Api,
    DeleteParams,
    ListParams,
};
use kube_runtime::wait::conditions;
use log::warn;
use log::{
    debug,
    error,
    info,
};
use rand::{
    distributions::Alphanumeric,
    Rng,
};
use tokio::task::JoinHandle;

use crate::client::create_client_with_specific_context;
use crate::client::{
    get_services_with_annotation,
    list_all_namespaces,
};
use crate::models::kube::{
    HttpLogState,
    Port,
    PortForward,
    Target,
    TargetSelector,
};
use crate::port_forward::CANCEL_NOTIFIER;
use crate::port_forward::CHILD_PROCESSES;

pub async fn start_port_forward(
    configs: Vec<Config>, protocol: &str, http_log_state: Arc<HttpLogState>,
) -> Result<Vec<CustomResponse>, String> {
    let mut responses = Vec::new();
    let mut errors = Vec::new();
    let mut child_handles = Vec::new();

    for config in configs.iter() {
        let selector = match config.workload_type.as_deref() {
            Some("pod") => TargetSelector::PodLabel(config.target.clone().unwrap_or_default()),
            _ => TargetSelector::ServiceName(config.service.clone().unwrap_or_default()),
        };

        let remote_port = Port::from(config.remote_port.unwrap_or_default() as i32);
        let context_name = Some(config.context.clone());
        let kubeconfig = Some(config.kubeconfig.clone());
        let namespace = config.namespace.clone();
        let target = Target::new(selector, remote_port, namespace.clone());

        log::debug!("Remote Port: {:?}", config.remote_port);
        log::debug!("Local Port: {:?}", config.local_port);
        if config.workload_type.as_deref() == Some("pod") {
            log::info!("Attempting to forward to pod label: {:?}", &config.target);
        } else {
            log::info!("Attempting to forward to service: {:?}", &config.service);
        }

        let local_address_clone = config.local_address.clone();

        let port_forward_result: Result<PortForward, anyhow::Error> = PortForward::new(
            target,
            config.local_port,
            local_address_clone,
            context_name,
            kubeconfig.flatten(),
            config.id.unwrap_or_default(),
            config.workload_type.clone().unwrap_or_default(),
        )
        .await;

        match port_forward_result {
            Ok(port_forward) => {
                let forward_result = match protocol {
                    "udp" => port_forward.clone().port_forward_udp().await,
                    "tcp" => {
                        port_forward
                            .clone()
                            .port_forward_tcp(http_log_state.clone())
                            .await
                    }
                    _ => Err(anyhow::anyhow!("Unsupported protocol")),
                };

                match forward_result {
                    Ok((actual_local_port, handle)) => {
                        log::info!(
                            "{} port forwarding is set up on local port: {:?} for {}: {:?}",
                            protocol.to_uppercase(),
                            actual_local_port,
                            if config.workload_type.as_deref() == Some("pod") {
                                "pod label"
                            } else {
                                "service"
                            },
                            &config.service
                        );

                        debug!("Port forwarding details: {:?}", port_forward);
                        debug!("Actual local port: {:?}", actual_local_port);

                        let handle_key = format!(
                            "{}_{}",
                            config.id.unwrap(),
                            config.service.clone().unwrap_or_default()
                        );
                        CHILD_PROCESSES
                            .lock()
                            .unwrap()
                            .insert(handle_key.clone(), handle);
                        child_handles.push(handle_key.clone());

                        if config.domain_enabled.unwrap_or_default() {
                            let hostfile_comment = format!(
                                "kftray custom host for {} - {}",
                                config.service.clone().unwrap_or_default(),
                                config.id.unwrap_or_default()
                            );

                            let mut hosts_builder = HostsBuilder::new(hostfile_comment);

                            if let Some(service_name) = &config.service {
                                if let Some(local_address) = &config.local_address {
                                    match local_address.parse::<std::net::IpAddr>() {
                                        Ok(ip_addr) => {
                                            hosts_builder.add_hostname(
                                                ip_addr,
                                                config.alias.clone().unwrap_or_default(),
                                            );
                                            if let Err(e) = hosts_builder.write() {
                                                let error_message = format!(
                                                    "Failed to write to the hostfile for {}: {}",
                                                    service_name, e
                                                );
                                                log::error!("{}", &error_message);
                                                errors.push(error_message);

                                                if let Some(handle) = CHILD_PROCESSES
                                                    .lock()
                                                    .unwrap()
                                                    .remove(&handle_key)
                                                {
                                                    handle.abort();
                                                }
                                                continue;
                                            }
                                        }
                                        Err(_) => {
                                            let warning_message = format!(
                                                "Invalid IP address format: {}",
                                                local_address
                                            );
                                            log::warn!("{}", &warning_message);
                                            errors.push(warning_message);
                                        }
                                    }
                                }
                            }
                        }

                        let config_state = ConfigState {
                            id: None,
                            config_id: config.id.unwrap(),
                            is_running: true,
                        };
                        if let Err(e) = update_config_state(&config_state).await {
                            log::error!("Failed to update config state: {}", e);
                        }

                        responses.push(CustomResponse {
                            id: config.id,
                            service: config.service.clone().unwrap(),
                            namespace: namespace.clone(),
                            local_port: actual_local_port,
                            remote_port: config.remote_port.unwrap_or_default(),
                            context: config.context.clone(),
                            protocol: config.protocol.clone(),
                            stdout: format!(
                                "{} forwarding from 127.0.0.1:{} -> {:?}:{}",
                                protocol.to_uppercase(),
                                actual_local_port,
                                config.remote_port.unwrap_or_default(),
                                config.service.clone().unwrap()
                            ),
                            stderr: String::new(),
                            status: 0,
                        });
                    }
                    Err(e) => {
                        let error_message = format!(
                            "Failed to start {} port forwarding for {} {}: {}",
                            protocol.to_uppercase(),
                            if config.workload_type.as_deref() == Some("pod") {
                                "pod label"
                            } else {
                                "service"
                            },
                            config.service.clone().unwrap_or_default(),
                            e
                        );
                        log::error!("{}", &error_message);
                        errors.push(error_message);
                    }
                }
            }
            Err(e) => {
                let error_message = format!(
                    "Failed to create PortForward for {} {}: {}",
                    if config.workload_type.as_deref() == Some("pod") {
                        "pod label"
                    } else {
                        "service"
                    },
                    config.service.clone().unwrap_or_default(),
                    e
                );
                log::error!("{}", &error_message);
                errors.push(error_message);
            }
        }
    }

    if !errors.is_empty() {
        for handle_key in child_handles {
            if let Some(handle) = CHILD_PROCESSES.lock().unwrap().remove(&handle_key) {
                handle.abort();
            }
        }
        return Err(errors.join("\n"));
    }

    if !responses.is_empty() {
        log::debug!(
            "{} port forwarding responses generated successfully.",
            protocol.to_uppercase()
        );
    }

    Ok(responses)
}

pub async fn stop_all_port_forward() -> Result<Vec<CustomResponse>, String> {
    info!("Attempting to stop all port forwards");

    let mut responses = Vec::with_capacity(1024);
    CANCEL_NOTIFIER.notify_waiters();

    let handle_map: HashMap<String, JoinHandle<()>> = {
        let mut processes = CHILD_PROCESSES.lock().unwrap();
        processes.drain().collect()
    };

    let running_configs_state = match get_configs_state().await {
        Ok(states) => states
            .into_iter()
            .filter(|s| s.is_running)
            .map(|s| s.config_id)
            .collect::<Vec<i64>>(),
        Err(e) => {
            let error_message = format!("Failed to retrieve config states: {}", e);
            error!("{}", error_message);
            return Err(error_message);
        }
    };

    let configs = match kftray_commons::utils::config::get_configs().await {
        Ok(configs) => configs,
        Err(e) => {
            let error_message = format!("Failed to retrieve configs: {}", e);
            error!("{}", error_message);
            return Err(error_message);
        }
    };

    let config_map: HashMap<i64, &Config> = configs
        .iter()
        .filter_map(|c| c.id.map(|id| (id, c)))
        .collect();

    let empty_str = String::new();

    let mut abort_handles: FuturesUnordered<_> = handle_map
        .iter()
        .map(|(composite_key, handle)| {
            let ids: Vec<&str> = composite_key.split('_').collect();

            let empty_str_clone = empty_str.clone();
            let config_map_cloned = config_map.clone();

            async move {
                if ids.len() != 2 {
                    error!(
                        "Invalid composite key format encountered: {}",
                        composite_key
                    );
                    return CustomResponse {
                        id: None,
                        service: empty_str_clone.clone(),
                        namespace: empty_str_clone.clone(),
                        local_port: 0,
                        remote_port: 0,
                        context: empty_str_clone.clone(),
                        protocol: empty_str_clone.clone(),
                        stdout: empty_str_clone.clone(),
                        stderr: String::from("Invalid composite key format"),
                        status: 1,
                    };
                }

                let config_id_str = ids[0];
                let service_id = ids[1].to_string();
                let config_id_parsed = config_id_str.parse::<i64>().unwrap_or_default();
                let config_option = config_map_cloned.get(&config_id_parsed).cloned();

                if let Some(config) = config_option {
                    if config.domain_enabled.unwrap_or_default() {
                        let hostfile_comment =
                            format!("kftray custom host for {} - {}", service_id, config_id_str);
                        let hosts_builder = HostsBuilder::new(&hostfile_comment);

                        if let Err(e) = hosts_builder.write() {
                            error!("Failed to write to the hostfile for {}: {}", service_id, e);
                            return CustomResponse {
                                id: Some(config_id_parsed),
                                service: service_id.clone(),
                                namespace: empty_str_clone.clone(),
                                local_port: 0,
                                remote_port: 0,
                                context: empty_str_clone.clone(),
                                protocol: empty_str_clone.clone(),
                                stdout: empty_str_clone.clone(),
                                stderr: e.to_string(),
                                status: 1,
                            };
                        }
                    }
                } else {
                    warn!("Config with id '{}' not found.", config_id_str);
                }

                info!(
                    "Aborting port forwarding task for config_id: {}",
                    config_id_str
                );
                handle.abort();

                CustomResponse {
                    id: Some(config_id_parsed),
                    service: service_id,
                    namespace: empty_str_clone.clone(),
                    local_port: 0,
                    remote_port: 0,
                    context: empty_str_clone.clone(),
                    protocol: empty_str_clone.clone(),
                    stdout: String::from("Service port forwarding has been stopped"),
                    stderr: empty_str_clone,
                    status: 0,
                }
            }
        })
        .collect();

    while let Some(response) = abort_handles.next().await {
        responses.push(response);
    }

    let pod_deletion_tasks: FuturesUnordered<_> = configs
        .iter()
        .filter(|config| running_configs_state.contains(&config.id.unwrap_or_default()))
        .filter(|config| {
            config.protocol == "udp" || matches!(config.workload_type.as_deref(), Some("proxy"))
        })
        .filter_map(|config| {
            config
                .kubeconfig
                .as_ref()
                .map(|kubeconfig| (config, kubeconfig))
        })
        .map(|(config, kubeconfig)| {
            let config_id_str = config.id.unwrap_or_default();
            async move {
                match create_client_with_specific_context(
                    Some(kubeconfig.clone()),
                    Some(&config.context),
                )
                .await
                {
                    Ok((Some(client), _, _)) => {
                        let pods: Api<Pod> = Api::all(client.clone());
                        let lp =
                            ListParams::default().labels(&format!("config_id={}", config_id_str));

                        if let Ok(pod_list) = pods.list(&lp).await {
                            let username = whoami::username();
                            let pod_prefix = format!("kftray-forward-{}", username);
                            let delete_tasks: FuturesUnordered<_> = pod_list
                                .items
                                .into_iter()
                                .filter_map(|pod| {
                                    if let Some(pod_name) = pod.metadata.name {
                                        if pod_name.starts_with(&pod_prefix) {
                                            let namespace = pod
                                                .metadata
                                                .namespace
                                                .unwrap_or_else(|| "default".to_string());
                                            let pods_in_namespace: Api<Pod> =
                                                Api::namespaced(client.clone(), &namespace);
                                            let dp = DeleteParams {
                                                grace_period_seconds: Some(0),
                                                ..DeleteParams::default()
                                            };

                                            return Some(async move {
                                                match pods_in_namespace.delete(&pod_name, &dp).await
                                                {
                                                    Ok(_) => info!(
                                                        "Successfully deleted pod: {}",
                                                        pod_name
                                                    ),
                                                    Err(e) => error!(
                                                        "Failed to delete pod {}: {}",
                                                        pod_name, e
                                                    ),
                                                }
                                            });
                                        }
                                    }
                                    None
                                })
                                .collect();

                            delete_tasks.collect::<Vec<_>>().await;
                        } else {
                            error!("Error listing pods for config_id {}", config_id_str);
                        }
                    }
                    Ok((None, _, _)) => {
                        error!("Client not created for kubeconfig: {:?}", kubeconfig)
                    }
                    Err(e) => error!("Failed to create Kubernetes client: {}", e),
                }
            }
        })
        .collect();

    pod_deletion_tasks.collect::<Vec<_>>().await;

    let update_config_tasks: FuturesUnordered<_> = configs
        .iter()
        .map(|config| {
            let config_id_parsed = config.id.unwrap_or_default();
            async move {
                let config_state = ConfigState {
                    id: None,
                    config_id: config_id_parsed,
                    is_running: false,
                };
                if let Err(e) = update_config_state(&config_state).await {
                    error!("Failed to update config state: {}", e);
                } else {
                    info!(
                        "Successfully updated config state for config_id: {}",
                        config_id_parsed
                    );
                }
            }
        })
        .collect();

    update_config_tasks.collect::<Vec<_>>().await;

    info!(
        "Port forward stopping process completed with {} responses",
        responses.len()
    );

    Ok(responses)
}

pub async fn stop_port_forward(config_id: String) -> Result<CustomResponse, String> {
    let cancellation_notifier = CANCEL_NOTIFIER.clone();
    cancellation_notifier.notify_waiters();

    let composite_key = {
        let child_processes = CHILD_PROCESSES.lock().unwrap();
        child_processes
            .keys()
            .find(|key| key.starts_with(&format!("{}_", config_id)))
            .map(|key| key.to_string())
    };

    if let Some(composite_key) = composite_key {
        let join_handle = {
            let mut child_processes = CHILD_PROCESSES.lock().unwrap();
            debug!("child_processes: {:?}", child_processes);
            child_processes.remove(&composite_key)
        };

        if let Some(join_handle) = join_handle {
            debug!("Join handle: {:?}", join_handle);
            join_handle.abort();
        }

        let (config_id_str, service_name) = composite_key.split_once('_').unwrap_or(("", ""));
        let config_id_parsed = config_id_str.parse::<i64>().unwrap_or_default();

        match kftray_commons::config::get_configs().await {
            Ok(configs) => {
                if let Some(config) = configs
                    .iter()
                    .find(|c| c.id.map_or(false, |id| id == config_id_parsed))
                {
                    if config.domain_enabled.unwrap_or_default() {
                        let hostfile_comment = format!(
                            "kftray custom host for {} - {}",
                            service_name, config_id_str
                        );

                        let hosts_builder = HostsBuilder::new(hostfile_comment);

                        if let Err(e) = hosts_builder.write() {
                            log::error!(
                                "Failed to remove from the hostfile for {}: {}",
                                service_name,
                                e
                            );

                            let config_state = ConfigState {
                                id: None,
                                config_id: config_id_parsed,
                                is_running: false,
                            };
                            if let Err(e) = update_config_state(&config_state).await {
                                log::error!("Failed to update config state: {}", e);
                            }
                            return Err(e.to_string());
                        }
                    }
                } else {
                    log::warn!("Config with id '{}' not found.", config_id_str);
                }

                let config_state = ConfigState {
                    id: None,
                    config_id: config_id_parsed,
                    is_running: false,
                };
                if let Err(e) = update_config_state(&config_state).await {
                    log::error!("Failed to update config state: {}", e);
                }

                Ok(CustomResponse {
                    id: None,
                    service: service_name.to_string(),
                    namespace: String::new(),
                    local_port: 0,
                    remote_port: 0,
                    context: String::new(),
                    protocol: String::new(),
                    stdout: String::from("Service port forwarding has been stopped"),
                    stderr: String::new(),
                    status: 0,
                })
            }
            Err(e) => {
                let config_id_parsed = config_id.parse::<i64>().unwrap_or_default();
                let config_state = ConfigState {
                    id: None,
                    config_id: config_id_parsed,
                    is_running: false,
                };
                if let Err(e) = update_config_state(&config_state).await {
                    log::error!("Failed to update config state: {}", e);
                }
                Err(format!("Failed to retrieve configs: {}", e))
            }
        }
    } else {
        let config_id_parsed = config_id.parse::<i64>().unwrap_or_default();
        let config_state = ConfigState {
            id: None,
            config_id: config_id_parsed,
            is_running: false,
        };
        if let Err(e) = update_config_state(&config_state).await {
            log::error!("Failed to update config state: {}", e);
        }
        Err(format!(
            "No port forwarding process found for config_id '{}'",
            config_id
        ))
    }
}

fn render_json_template(template: &str, values: &HashMap<&str, String>) -> String {
    let mut rendered_template = template.to_string();

    for (key, value) in values.iter() {
        rendered_template = rendered_template.replace(&format!("{{{}}}", key), value);
    }

    rendered_template
}
pub async fn deploy_and_forward_pod(
    configs: Vec<Config>, http_log_state: Arc<HttpLogState>,
) -> Result<Vec<CustomResponse>, String> {
    let mut responses: Vec<CustomResponse> = Vec::new();

    for mut config in configs.into_iter() {
        let context_name = Some(config.context.as_str());
        let kubeconfig_clone = config.kubeconfig.clone();
        let (client, _, _) = create_client_with_specific_context(kubeconfig_clone, context_name)
            .await
            .map_err(|e| {
                log::error!("Failed to create Kubernetes client: {}", e);
                e.to_string()
            })?;

        let client = client.ok_or_else(|| "Client not created".to_string())?;

        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|e| e.to_string())?
            .as_secs();

        let random_string: String = rand::thread_rng()
            .sample_iter(&Alphanumeric)
            .take(6)
            .map(char::from)
            .map(|c| c.to_ascii_lowercase())
            .collect();

        let username = whoami::username().to_lowercase();
        let clean_username: String = username.chars().filter(|c| c.is_alphanumeric()).collect();

        info!("Cleaned username: {}", clean_username);

        let protocol = config.protocol.to_string().to_lowercase();

        let hashed_name = format!(
            "kftray-forward-{}-{}-{}-{}",
            clean_username, protocol, timestamp, random_string
        )
        .to_lowercase();

        let config_id_str = config
            .id
            .map_or_else(|| "default".into(), |id| id.to_string());

        if config
            .remote_address
            .as_ref()
            .map_or(true, String::is_empty)
        {
            config.remote_address.clone_from(&config.service)
        }

        let mut values: HashMap<&str, String> = HashMap::new();
        values.insert("hashed_name", hashed_name.clone());
        values.insert("config_id", config_id_str);
        values.insert("service_name", config.service.as_ref().unwrap().clone());
        values.insert(
            "remote_address",
            config.remote_address.as_ref().unwrap().clone(),
        );
        values.insert("remote_port", config.remote_port.expect("None").to_string());
        values.insert("local_port", config.remote_port.expect("None").to_string());
        values.insert("protocol", protocol.clone());

        let manifest_path = get_pod_manifest_path().map_err(|e| e.to_string())?;
        let mut file = File::open(manifest_path).map_err(|e| e.to_string())?;
        let mut contents = String::new();
        file.read_to_string(&mut contents)
            .map_err(|e| e.to_string())?;

        let rendered_json = render_json_template(&contents, &values);
        let pod: Pod = serde_json::from_str(&rendered_json).map_err(|e| e.to_string())?;

        let pods: Api<Pod> = Api::namespaced(client.clone(), &config.namespace);

        match pods.create(&kube::api::PostParams::default(), &pod).await {
            Ok(_) => {
                if let Err(e) = kube_runtime::wait::await_condition(
                    pods.clone(),
                    &hashed_name,
                    conditions::is_pod_running(),
                )
                .await
                {
                    let dp = DeleteParams {
                        grace_period_seconds: Some(0),
                        ..DeleteParams::default()
                    };
                    let _ = pods.delete(&hashed_name, &dp).await;
                    return Err(e.to_string());
                }

                config.service = Some(hashed_name.clone());

                let start_response = match protocol.as_str() {
                    "udp" => {
                        start_port_forward(vec![config.clone()], "udp", http_log_state.clone())
                            .await
                    }
                    "tcp" => {
                        start_port_forward(vec![config.clone()], "tcp", http_log_state.clone())
                            .await
                    }
                    _ => {
                        let _ = pods
                            .delete(&hashed_name, &kube::api::DeleteParams::default())
                            .await;
                        return Err("Unsupported proxy type".to_string());
                    }
                };

                match start_response {
                    Ok(mut port_forward_responses) => {
                        let response = port_forward_responses
                            .pop()
                            .ok_or("No response received from port forwarding")?;
                        responses.push(response);
                    }
                    Err(e) => {
                        let _ = pods
                            .delete(&hashed_name, &kube::api::DeleteParams::default())
                            .await;
                        return Err(format!("Failed to start port forwarding {}", e));
                    }
                }
            }
            Err(e) => return Err(e.to_string()),
        }
    }

    Ok(responses)
}

pub async fn stop_proxy_forward(
    config_id: i64, namespace: &str, service_name: String,
) -> Result<CustomResponse, String> {
    info!(
        "Attempting to stop proxy forward for service: {}",
        service_name
    );

    let config = get_config(config_id).await.map_err(|e| {
        error!("Failed to get config: {}", e);
        e.to_string()
    })?;

    let kubeconfig = config
        .kubeconfig
        .ok_or_else(|| "Kubeconfig not found".to_string())?;
    let context_name = &config.context;

    let (client, _, _) = create_client_with_specific_context(Some(kubeconfig), Some(context_name))
        .await
        .map_err(|e| {
            error!("Failed to create Kubernetes client: {}", e);
            e.to_string()
        })?;

    let client = client.ok_or_else(|| "Client not created".to_string())?;

    let pods: Api<Pod> = Api::namespaced(client, namespace);

    let lp = ListParams::default().labels(&format!("config_id={}", config_id));

    let pod_list = pods.list(&lp).await.map_err(|e| {
        error!("Error listing pods: {}", e);
        e.to_string()
    })?;

    let username = whoami::username();

    let pod_prefix = format!("kftray-forward-{}", username);

    debug!("Looking for pods with prefix: {}", pod_prefix);

    for pod in pod_list.items {
        if let Some(pod_name) = pod.metadata.name {
            if pod_name.starts_with(&pod_prefix) {
                info!("Found pod to stop: {}", pod_name);

                let delete_options = DeleteParams {
                    grace_period_seconds: Some(0),
                    propagation_policy: Some(kube::api::PropagationPolicy::Background),
                    ..Default::default()
                };

                match pods.delete(&pod_name, &delete_options).await {
                    Ok(_) => info!("Successfully deleted pod: {}", pod_name),
                    Err(e) => {
                        error!("Failed to delete pod: {} with error: {}", pod_name, e);
                        return Err(e.to_string());
                    }
                }

                break;
            } else {
                info!("Pod {} does not match prefix, skipping", pod_name);
            }
        }
    }

    info!("Stopping port forward for service: {}", service_name);

    let stop_result = stop_port_forward(config_id.to_string())
        .await
        .map_err(|e| {
            error!(
                "Failed to stop port forwarding for service '{}': {}",
                service_name, e
            );
            e
        })?;

    info!("Proxy forward stopped for service: {}", service_name);

    Ok(stop_result)
}

pub async fn retrieve_service_configs(
    context: &str, kubeconfig: Option<String>,
) -> Result<Vec<Config>, String> {
    let (client_opt, _, _) = create_client_with_specific_context(kubeconfig.clone(), Some(context))
        .await
        .map_err(|e| e.to_string())?;

    let client = client_opt.ok_or_else(|| "Client not created".to_string())?;
    let annotation = "kftray.app/configs";

    let namespaces = list_all_namespaces(client.clone())
        .await
        .map_err(|e| e.to_string())?;

    let concurrency_limit = 10;

    stream::iter(namespaces)
        .map(|namespace| {
            let client = client.clone();
            let context = context.to_string();
            let kubeconfig = kubeconfig.clone();
            let annotation = annotation.to_string();
            async move {
                let services =
                    get_services_with_annotation(client.clone(), &namespace, &annotation)
                        .await
                        .map_err(|e| e.to_string())?;

                let mut namespace_configs = Vec::new();

                for (service_name, annotations, ports) in services {
                    if let Some(configs_str) = annotations.get(&annotation) {
                        namespace_configs.extend(parse_configs(
                            configs_str,
                            &context,
                            &namespace,
                            &service_name,
                            &ports,
                            kubeconfig.clone(),
                        ));
                    } else {
                        namespace_configs.extend(create_default_configs(
                            &context,
                            &namespace,
                            &service_name,
                            &ports,
                            kubeconfig.clone(),
                        ));
                    }
                }

                Ok(namespace_configs)
            }
        })
        .buffer_unordered(concurrency_limit)
        .fold(
            Ok(Vec::new()),
            |mut acc: Result<Vec<Config>, String>, result: Result<Vec<Config>, String>| async {
                match result {
                    Ok(mut namespace_configs) => {
                        acc.as_mut().unwrap().append(&mut namespace_configs)
                    }
                    Err(e) => {
                        eprintln!("Error processing namespace: {}", e);
                    }
                }
                acc
            },
        )
        .await
}

fn parse_configs(
    configs_str: &str, context: &str, namespace: &str, service_name: &str,
    ports: &HashMap<String, i32>, kubeconfig: Option<String>,
) -> Vec<Config> {
    configs_str
        .split(',')
        .filter_map(|config_str| {
            let parts: Vec<&str> = config_str.trim().split('-').collect();
            if parts.len() != 3 {
                return None;
            }

            let alias = parts[0].to_string();
            let local_port: u16 = parts[1].parse().ok()?;
            let target_port = parts[2]
                .parse()
                .ok()
                .or_else(|| ports.get(parts[2]).cloned())?;

            Some(Config {
                id: None,
                context: context.to_string(),
                kubeconfig: kubeconfig.clone(),
                namespace: namespace.to_string(),
                service: Some(service_name.to_string()),
                alias: Some(alias),
                local_port: Some(local_port),
                remote_port: Some(target_port as u16),
                protocol: "tcp".to_string(),
                workload_type: Some("service".to_string()),
                ..Default::default()
            })
        })
        .collect()
}

fn create_default_configs(
    context: &str, namespace: &str, service_name: &str, ports: &HashMap<String, i32>,
    kubeconfig: Option<String>,
) -> Vec<Config> {
    ports
        .iter()
        .map(|(_port_name, &port)| Config {
            id: None,
            context: context.to_string(),
            kubeconfig: kubeconfig.clone(),
            namespace: namespace.to_string(),
            service: Some(service_name.to_string()),
            alias: Some(service_name.to_string()),
            local_port: Some(port as u16),
            remote_port: Some(port as u16),
            protocol: "tcp".to_string(),
            workload_type: Some("service".to_string()),
            ..Default::default()
        })
        .collect()
}
