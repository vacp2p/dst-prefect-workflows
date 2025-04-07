from prefect import flow, task
from prefect.server.schemas.schedules import IntervalSchedule
from datetime import timedelta
import base64
import requests
import json
import os
from dotenv import load_dotenv
import time

# Load environment variables from .env file
load_dotenv()

AUTHORIZED_USERS = ["zorlin", "AlbertoSoutullo", "michatinkers"]

LARS_API_URL = os.getenv("LARS_API_URL", "http://localhost:9930")

@task
def find_valid_issue(repo_name: str, github_token: str):
    url = f"https://api.github.com/repos/{repo_name}/issues"
    headers = {"Authorization": f"token {github_token}"}
    response = requests.get(url, headers=headers)

    if response.status_code != 200:
        print(f"Failed to fetch issues. Status code: {response.status_code}")
        return "NOT_VERIFIED", None

    issues = response.json()
    print(f"Found {len(issues)} issues")
    
    for issue in issues:
        print(f"Checking issue #{issue['number']}")
        labels = [l['name'] for l in issue.get('labels', [])]
        print(f"Issue labels: {labels}")
        
        # Skip if already done
        if "simulation-done" in labels:
            print(f"Issue #{issue['number']} already marked as done")
            continue
            
        # Check if needs scheduling
        if "needs-scheduling" not in labels:
            print(f"Issue #{issue['number']} doesn't need scheduling")
            continue

        events_url = f"https://api.github.com/repos/{repo_name}/issues/{issue['number']}/events"
        events = requests.get(events_url, headers=headers).json()
        print(f"Found {len(events)} events for issue #{issue['number']}")
        
        for e in reversed(events):
            if e['event'] == 'labeled' and e['label']['name'] == 'needs-scheduling':
                actor = e['actor']['login'].lower()
                print(f"Found needs-scheduling label added by {actor}")
                if actor in [u.lower() for u in AUTHORIZED_USERS]:
                    print(f"Authorized user {actor} added the label")
                    encoded = base64.b64encode(json.dumps(issue).encode()).decode()
                    return "VERIFIED", encoded
                else:
                    print(f"User {actor} is not authorized")
    return "NOT_VERIFIED", None

@task
def parse_and_generate_matrix(valid_issue_encoded: str):
    try:
        issue = json.loads(base64.b64decode(valid_issue_encoded).decode())
        body = issue['body']
        issue_number = issue['number']
    except:
        return []

    lines = body.split('\n')
    data = {}
    current = None
    for line in lines:
        if line.startswith("### "):
            current = line[4:].strip()
            data[current] = ""
        elif current:
            data[current] += line + "\n"

    # Utility function to handle "_No response_" and empty values
    def get_valid_value(key, default_value=""):
        value = data.get(key, default_value).strip()
        if not value or value == "_No response_":
            return default_value
        return value
    
    def parse_list(val, default="0"):
        if not val or val == "_No response_":
            val = default
        return [int(v.strip()) for v in val.split(",") if v.strip().isdigit()]
    
    def parse_string_list(val, default=""):
        if not val or val == "_No response_":
            val = default
        return [v.strip() for v in val.split(",") if v.strip()]
    
    def safe_int(val, default):
        if not val or val == "_No response_":
            return default
        try:
            return int(val.strip()) if val and val.strip() else default
        except (ValueError, TypeError):
            return default
    
    def safe_bool(val, default=False):
        if not val or val == "_No response_":
            return default
        return val.lower() == "yes"

    # Get the program being tested
    program = get_valid_value("What program does this test concern?", "").lower()
    if program not in ["waku", "nimlibp2p"]:
        print(f"Unknown program: {program}. Defaulting to waku.")
        program = "waku"

    # Common parameters for both charts
    parallel_runs = parse_list(get_valid_value("Parallelism", "1"))
    parallel_limit = parallel_runs[0] if parallel_runs else 1
    docker_images = parse_string_list(get_valid_value("Docker image", "statusteam/nim-waku:latest"))

    matrix = []
    i = 0

    if program == "waku":
        # Waku-specific parameters
        nodes = parse_list(get_valid_value("Number of nodes", "50"))
        durations = parse_list(get_valid_value("Duration", "5"))
        bootstrap_nodes = safe_int(get_valid_value("Bootstrap nodes"), 3)
        
        # Handle pubsub topic with validation
        raw_pubsub_topic = get_valid_value("PubSub Topic")
        if not raw_pubsub_topic or not raw_pubsub_topic.startswith("/waku/2/rs"):
            pubsub_topic = "/waku/2/rs/2/0"  # Default topic that is properly formatted
            print(f"Using default pubsub topic '{pubsub_topic}' because the provided value was invalid or missing")
        else:
            pubsub_topic = raw_pubsub_topic
            print(f"Using provided pubsub topic: {pubsub_topic}")
        
        publisher_enabled = safe_bool(get_valid_value("Enable Publisher"))
        
        # Matrix parameters for publisher settings
        publisher_message_sizes = parse_list(get_valid_value("Publisher Message Size"), "1")
        publisher_delays = parse_list(get_valid_value("Publisher Delay"), "10")
        publisher_message_counts = parse_list(get_valid_value("Publisher Message Count"), "1000")
        
        artificial_latency = safe_bool(get_valid_value("Enable Artificial Latency"))
        latency_ms = safe_int(get_valid_value("Artificial Latency (ms)"), 50)
        
        nodes_command = get_valid_value("Nodes Command")
        bootstrap_command = get_valid_value("Bootstrap Command")

        # Generate matrix for all combinations
        for n in nodes:
            for d in durations:
                for docker_image in docker_images:
                    if publisher_enabled:
                        # If publisher is enabled, generate all combinations with publisher parameters
                        for msg_size in publisher_message_sizes:
                            for delay in publisher_delays:
                                for msg_count in publisher_message_counts:
                                    matrix.append({
                                        "index": i,
                                        "issue_number": issue_number,
                                        "chart": "waku",
                                        "nodecount": n,
                                        "duration": d,
                                        "bootstrap_nodes": bootstrap_nodes,
                                        "docker_image": docker_image,
                                        "pubsub_topic": pubsub_topic,
                                        "publisher_enabled": publisher_enabled,
                                        "publisher_message_size": msg_size,
                                        "publisher_delay": delay,
                                        "publisher_message_count": msg_count,
                                        "artificial_latency": artificial_latency,
                                        "latency_ms": latency_ms,
                                        "nodes_command": nodes_command,
                                        "bootstrap_command": bootstrap_command,
                                        "parallel_limit": parallel_limit
                                    })
                                    i += 1
                                    print(f"Generated {i} matrix entries")
                    else:
                        # If publisher is disabled, generate one entry without publisher parameters
                        matrix.append({
                            "index": i,
                            "issue_number": issue_number,
                            "chart": "waku",
                            "nodecount": n,
                            "duration": d,
                            "bootstrap_nodes": bootstrap_nodes,
                            "docker_image": docker_image,
                            "pubsub_topic": pubsub_topic,
                            "publisher_enabled": publisher_enabled,
                            "publisher_message_size": 0,
                            "publisher_delay": 0,
                            "publisher_message_count": 0,
                            "artificial_latency": artificial_latency,
                            "latency_ms": latency_ms,
                            "nodes_command": nodes_command,
                            "bootstrap_command": bootstrap_command,
                            "parallel_limit": parallel_limit
                        })
                        i += 1
                        print(f"Generated {i} matrix entries")
    elif program == "nimlibp2p":
        # nimlibp2p-specific parameters
        peer_number = safe_int(get_valid_value("Peer number"), 0)
        number_of_peers = safe_int(get_valid_value("Number of peers"), 1000)
        peers_to_connect = safe_int(get_valid_value("Peers to connect to"), 10)
        message_rate = safe_int(get_valid_value("Message rate"), 1000)
        message_size = safe_int(get_valid_value("Message size"), 100)
        duration = safe_int(get_valid_value("Duration"), 5)

        matrix.append({
            "index": i,
            "issue_number": issue_number,
            "chart": "nimlibp2p",
            "peer_number": peer_number,
            "number_of_peers": number_of_peers,
            "peers_to_connect": peers_to_connect,
            "message_rate": message_rate,
            "message_size": message_size,
            "duration": duration,
            "docker_image": docker_images[0],
            "parallel_limit": parallel_limit
        })
        i += 1
        print(f"Generated {i} matrix entries")
    else:
        print(f"Unknown program: {program} for issue {issue_number}")
        return []

    return matrix

@task(retries=3, retry_delay_seconds=10)
def call_lars_api(endpoint: str, payload: dict):
    url = f"{LARS_API_URL}{endpoint}"
    headers = {"Content-Type": "application/json"}
    try:
        response = requests.post(url, headers=headers, json=payload, timeout=30)
        response.raise_for_status()
        return response.json()
    except requests.exceptions.RequestException as e:
        print(f"Error calling LARS API {endpoint}: {e}")
        raise

@task
def deploy_config(config: dict):
    print(f"Processing config for chart={config.get('chart', 'unknown')}, nodes={config.get('nodecount', config.get('number_of_peers', 'N/A'))}")
    
    # --- 1. Request Run from LARS --- 
    node_count = config.get("nodecount", config.get("number_of_peers", 50))
    duration_minutes = config.get("duration", 5)
    duration_secs = duration_minutes * 60
    chart_type = config.get("chart", "waku")
    
    request_payload = {
        "chart": chart_type,
        "node_count": node_count,
        "duration_secs": duration_secs,
        # LARS will predict cost based on this info
    }
    
    simulation_id = None
    accepted = False
    poll_interval = 60
    
    while not accepted:
        print(f"Requesting run approval from LARS for config index {config.get('index', 'N/A')}...")
        try:
            response = call_lars_api("/api/v1/request_run", request_payload)
            if response.get("status") == "ACCEPTED":
                accepted = True
                simulation_id = response.get("simulation_id")
                print(f"LARS ACCEPTED run. Simulation ID: {simulation_id}")
            else:
                reason = response.get("reason", "No reason provided")
                print(f"LARS REJECTED run: {reason}. Retrying in {poll_interval}s...")
                time.sleep(poll_interval)
        except Exception as e:
            print(f"Error requesting run from LARS: {e}. Retrying in {poll_interval}s...")
            time.sleep(poll_interval)
            
    if not simulation_id:
        print("Error: Run accepted by LARS but no Simulation ID received. Aborting.")
        return None

    # --- Continue with deployment logic only if accepted --- 
    import subprocess
    import yaml
    import re
    from datetime import datetime

    # Helper function to process args properly
    def process_args(args_str):
        if not args_str:
            return []
        
        # Preserve template expressions by temporarily replacing them
        placeholders = {}
        pattern = r'({{[^}]+}})'
        
        def replace_templates(match):
            placeholder = f"TEMPLATE_PLACEHOLDER_{len(placeholders)}"
            placeholders[placeholder] = match.group(0)
            return placeholder
        
        # Replace template expressions with placeholders
        processed_str = re.sub(pattern, replace_templates, args_str)
        
        # Split on whitespace
        parts = processed_str.split()
        
        # Restore template expressions
        for i, part in enumerate(parts):
            for placeholder, template in placeholders.items():
                if placeholder in part:
                    parts[i] = part.replace(placeholder, template)
                    
        return parts
    
    # Extract common values
    index = config.get("index", "unknown")
    docker_image = config.get("docker_image", "statusteam/nim-waku:latest")
    chart = config.get("chart", "waku")
    
    # Generate descriptive release name
    if chart == "waku":
        nodecount = config.get("nodecount", 50)
        message_rate = 1000 // config.get("publisher_delay", 10)  # messages per second
        message_size = config.get("publisher_message_size", 1)
        k_value = nodecount/1000
        k_str = f"{int(k_value)}K" if k_value >= 1 else f"{int(nodecount)}"
        release_name = f"waku-{k_str}-{message_rate}mgs-{config.get('publisher_delay', 10)}s-{message_size}kb-{index}"
    else:  # nimlibp2p
        peer_number = config.get("peer_number", 0)
        message_rate = config.get("message_rate", 1000)
        message_size = config.get("message_size", 100)
        k_value = peer_number/1000
        k_str = f"{int(k_value)}K" if k_value >= 1 else f"{int(peer_number)}"
        release_name = f"nimlibp2p-{k_str}-{message_rate}mgs-{message_size}KB-{index}"
    
    print(f"Deploying configuration: {release_name} (LARS Sim ID: {simulation_id})")
    
    # Generate values.yaml based on chart type
    if chart == "waku":
        values = {
            'global': {
                'pubSubTopic': config.get("pubsub_topic", "/waku/2/rs/2/0")
            },
            'replicaCount': {
                'bootstrap': config.get("bootstrap_nodes", 3),
                'nodes': config.get("nodecount", 50)
            },
            'image': {
                'repository': docker_image.split(':')[0] if ':' in docker_image else docker_image,
                'tag': docker_image.split(':')[1] if ':' in docker_image else 'latest',
                'pullPolicy': 'IfNotPresent'
            },
            'bootstrap': {
                'command': [config.get("bootstrap_command")] if config.get("bootstrap_command") and isinstance(config.get("bootstrap_command"), str) else config.get("bootstrap_command", []),
                'resources': {
                    'requests': {
                        'memory': "64Mi",
                        'cpu': "50m"
                    },
                    'limits': {
                        'memory': "768Mi",
                        'cpu': "400m"
                    }
                }
            },
            'nodes': {
                'command': [config.get("nodes_command")] if config.get("nodes_command") and isinstance(config.get("nodes_command"), str) else config.get("nodes_command", []),
                'resources': {
                    'requests': {
                        'memory': "64Mi",
                        'cpu': "150m"
                    },
                    'limits': {
                        'memory': "600Mi",
                        'cpu': "500m"
                    }
                }
            },
            'publisher': {
                'enabled': config.get("publisher_enabled", False),
                'image': {
                    'repository': 'zorlin/publisher',
                    'tag': 'v0.5.0'
                },
                'messageSize': config.get("publisher_message_size", 1),
                'delaySeconds': config.get("publisher_delay", 10),
                'messageCount': config.get("publisher_message_count", 1000),
                'startDelay': {
                    'enabled': False,
                    'minutes': 5
                },
                'waitForStatefulSet': {
                    'enabled': True,
                    'stabilityMinutes': 1
                }
            },
            'artificialLatency': {
                'enabled': config.get("artificial_latency", False),
                'latencyMs': config.get("latency_ms", 50)
            }
        }
    else:  # nimlibp2p
        values = {
            'image': {
                'repository': docker_image.split(':')[0] if ':' in docker_image else docker_image,
                'tag': docker_image.split(':')[1] if ':' in docker_image else 'latest',
                'pullPolicy': 'IfNotPresent'
            },
            'config': {
                'peerNumber': config.get("peer_number", 0),
                'numberOfPeers': config.get("number_of_peers", 1000),
                'peersToConnect': config.get("peers_to_connect", 10),
                'messageRate': config.get("message_rate", 1000),
                'messageSize': config.get("message_size", 100)
            },
            'resources': {
                'requests': {
                    'memory': "64Mi",
                    'cpu': "150m"
                },
                'limits': {
                    'memory': "600Mi",
                    'cpu': "500m"
                }
            }
        }
    
    # Write values.yaml to a temporary file
    values_file = f"/tmp/values-{release_name}.yaml"
    with open(values_file, 'w') as f:
        yaml.dump(values, f)
    
    # Check if helm is installed, install if not
    try:
        subprocess.run(["helm", "--help"], stdout=subprocess.PIPE, stderr=subprocess.PIPE, check=True)
        print("Helm is already installed")
    except (subprocess.SubprocessError, FileNotFoundError):
        print("Installing Helm...")
        subprocess.run("curl -fsSL -o get_helm.sh https://raw.githubusercontent.com/helm/helm/main/scripts/get-helm-3", shell=True)
        subprocess.run("chmod 700 get_helm.sh", shell=True)
        subprocess.run("./get_helm.sh", shell=True)
    
    # Deploy with Helm
    namespace = "larstesting"
    chart_version = "0.4.6" if chart == "waku" else "0.1.0"
    chart_url = f"https://github.com/vacp2p/dst-argo-workflows/raw/refs/heads/main/charts/{chart}-{chart_version}.tgz"
    
    # Create namespace if it doesn't exist
    try:
        subprocess.run(["kubectl", "create", "namespace", namespace], 
                      stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        print(f"Created namespace {namespace}")
    except subprocess.SubprocessError as e:
        print(f"Note: {namespace} namespace might already exist: {e}")
        
    helm_cmd = [
        "helm", "upgrade", "--install", release_name,
        chart_url,
        "-f", values_file,
        "--namespace", namespace
    ]
    
    print(f"Running Helm command: {' '.join(helm_cmd)}")
    start_time_deploy = datetime.now()
    deploy_success = False
    try:
        deploy_result = subprocess.run(helm_cmd, capture_output=True, text=True, check=True)
        print(f"Helm deployment initiated successfully: {deploy_result.stdout}")
        deploy_success = True
    except subprocess.CalledProcessError as e:
        print(f"Error initiating Helm deployment: {e.stderr}")
        print(f"Helm output: {e.stdout}")
        return None

    # --- 2. Report Start to LARS --- 
    if deploy_success:
        print(f"Reporting start to LARS for Simulation ID: {simulation_id}")
        # Payload now only contains ID and descriptive info
        report_start_payload = {
            "simulation_id": simulation_id,
            # LARS already knows the predicted cost from the request_run phase
            # "actual_cpu": ???,  <-- Removed
            # "actual_memory": ???, <-- Removed
            "chart": chart_type,
            "node_count": node_count,
            "duration_secs": duration_secs,
        }
        try:
            call_lars_api("/api/v1/report_start", report_start_payload)
            print("Successfully reported start to LARS.")
        except Exception as e:
            print(f"Warning: Failed to report start to LARS: {e}. Continuing simulation run.")
            # Consider adding logic here if reporting start is absolutely critical

    # Wait for StatefulSet (if deploy was initiated)
    if deploy_success:
        print(f"Waiting for StatefulSet {release_name}-nodes to be ready...")
        rollout_cmd = [
            "kubectl", "rollout", "status",
            "--watch",
            "--timeout=300s",
            f"statefulset/{release_name}-nodes",
            "-n", namespace
        ]
        try:
            rollout_result = subprocess.run(rollout_cmd, capture_output=True, text=True, check=True)
            print(f"StatefulSet {release_name}-nodes is ready.")
            start_time_actual = datetime.now().strftime("%Y-%m-%d %H:%M:%S")
        except subprocess.CalledProcessError as e:
            print(f"Error waiting for StatefulSet: {e.stderr}. Proceeding with cleanup.")
            start_time_actual = start_time_deploy.strftime("%Y-%m-%d %H:%M:%S")
    else:
        print("Skipping rollout wait as deployment failed to initiate.")
        start_time_actual = start_time_deploy.strftime("%Y-%m-%d %H:%M:%S")
    
    # Wait for specified duration (only if deployment was attempted)
    end_time = datetime.now().strftime("%Y-%m-%d %H:%M:%S")
    if deploy_success:
        duration_seconds = config.get("duration", 5) * 60
        print(f"Waiting for simulation duration: {config.get('duration', 5)} minutes ({duration_seconds} seconds)...")
        
        chunk_size = 60 
        chunks = duration_seconds // chunk_size
        remainder = duration_seconds % chunk_size
        for i in range(chunks):
            time.sleep(chunk_size)
            print(f"Progress: {(i+1)*chunk_size}/{duration_seconds} seconds elapsed")
        if remainder > 0:
            time.sleep(remainder)
            print(f"Progress: {duration_seconds}/{duration_seconds} seconds elapsed")
        
        end_time = datetime.now().strftime("%Y-%m-%d %H:%M:%S")
        print(f"Finished simulation duration at: {end_time}")
    else:
        print("Skipping simulation duration wait as deployment failed.")

    # --- 3. Report Completion to LARS --- 
    print(f"Reporting completion to LARS for Simulation ID: {simulation_id}")
    report_complete_payload = {
        "simulation_id": simulation_id,
        "final_cpu_cores": None,
        "final_memory_gb": None,
    }
    try:
        call_lars_api("/api/v1/report_complete", report_complete_payload)
        print("Successfully reported completion to LARS.")
    except Exception as e:
        print(f"Warning: Failed to report completion to LARS: {e}. State in LARS might be inconsistent.")

    # Cleanup (always attempt cleanup)
    print(f"Cleaning up deployment {release_name}...")
    cleanup_cmd = ["helm", "uninstall", release_name, "--namespace", namespace]
    try:
        cleanup_result = subprocess.run(cleanup_cmd, capture_output=True, text=True, check=True)
        print(f"Successfully cleaned up deployment {release_name}.")
    except subprocess.CalledProcessError as e:
        print(f"Warning: Error during Helm cleanup: {e.stderr}")

    # Return data needed for analysis (even if deployment failed, might have partial data)
    simulation_data_result = [
        start_time_actual, 
        end_time, 
        release_name
    ]
    
    return simulation_data_result

@task
def run_analysis(simulation_data: list):
    import subprocess
    import os
    
    # Clone and run analysis for all simulations
    print("Cloning 10ksim repository...")
    try:
        # Check if already cloned
        if not os.path.exists("10ksim"):
            clone_cmd = ["git", "clone", "https://github.com/vacp2p/10ksim.git"]
            subprocess.run(clone_cmd, check=True)

        print("Generating and running analysis scripts...")
        analysis_script = f"""# Python Imports

# Project Imports
import src.logger.logger
from src.metrics.scrapper import Scrapper
from src.plotting.plotter import Plotter
from src.utils import file_utils



def main():
    url = "https://metrics.riff.cc/select/0/prometheus/api/v1/"
    scrape_config = "scrape.yaml"

    scrapper = Scrapper("ruby.yaml", url, scrape_config)
    scrapper.query_and_dump_metrics()

    config_dict = file_utils.read_yaml_file("scrape.yaml")
    plotter = Plotter(config_dict["plotting"])
    plotter.create_plots()


if __name__ == '__main__':
    main()
        """
            
        analysis_file = f"10ksim/analyse.py"
        with open(analysis_file, "w") as f:
            f.write(analysis_script)
        
        print(f"Analysis script generated at {analysis_file}")

        # Run analysis script once
        print("Running analysis script...")
        analysis_run_cmd = ["python3", f"10ksim/analyse.py"]
        try:
            analysis_result = subprocess.run(analysis_run_cmd, capture_output=True, text=True, check=True)
            print("Analysis complete. Output:")
            print(analysis_result.stdout)
        except subprocess.CalledProcessError as e:
            print(f"Warning: Error during analysis: {e.stderr}")
            print("Analysis may require manual execution.")
    except Exception as e:
        print(f"Error during repository cloning or analysis: {e}")
        print("You may need to manually clone the repository and run the analysis scripts.")

@task
def generate_scrape_yaml(simulation_data: list):
    import yaml
    
    # Generate scrape.yaml with all simulation data
    scrape_config = {
        "general_config": {
            "times_names": simulation_data  # Use all collected simulation data
        },
        "scrape_config": {
            "$__rate_interval": "121s",
            "step": "60s",
            "dump_location": "test/nwaku0.26-f/"
        },
        "metrics_to_scrape": {
            "libp2p_network_in": {
                "query": "rate(libp2p_network_bytes_total{direction='in'}[$__rate_interval])",
                "extract_field": "instance",
                "folder_name": "libp2p-in/"
            },
            "libp2p_network_out": {
                "query": "rate(libp2p_network_bytes_total{direction='out'}[$__rate_interval])",
                "extract_field": "instance",
                "folder_name": "libp2p-out/"
            }
        },
        "plotting": {
            "bandwidth-0-33-3K": {
                "ignore_columns": ["bootstrap", "midstrap"],
                "data_points": 25,
                "folder": ["test/nwaku0.26-f/"],
                "data": ["libp2p-in", "libp2p-out"],
                "include_files": [d[2] for d in simulation_data],  # Use all release names
                "xlabel_name": "Simulation",
                "ylabel_name": "KBytes/s",
                "show_min_max": False,
                "outliers": True,
                "scale-x": 1000,
                "fig_size": [20, 20]
            }
        }
    }

    # Write scrape.yaml with custom formatting for times_names
    with open("scrape.yaml", 'w') as f:
        # First write the general_config section with times_names as arrays
        f.write("general_config:\n")
        f.write("  times_names:\n")
        for data in simulation_data:
            f.write(f"  - {data}\n")
        
        # Then write the rest of the config
        f.write("\n")
        yaml.dump(
            {k: v for k, v in scrape_config.items() if k != "general_config"},
            f,
            default_flow_style=False,
            sort_keys=False,
            allow_unicode=True
        )

@flow
def deployment_cron_job(repo_name: str, github_token: str):
    result, valid_issue = find_valid_issue(repo_name, github_token)
    if result == "VERIFIED" and valid_issue:
        matrix = parse_and_generate_matrix(valid_issue)
        
        parallel_limit = 1
        if matrix:
            parallel_limit = matrix[0].get("parallel_limit", 1)
        
        print(f"Running with parallelism limit of {parallel_limit}")
        
        active_futures = []
        simulation_results = []
        failed_deployments = 0
        
        for config in matrix:
            while len(active_futures) >= parallel_limit:
                try:
                    completed_future = active_futures.pop(0)
                    result = completed_future.result()
                    if result:
                        simulation_results.append(result)
                    else:
                        failed_deployments += 1
                except Exception as e:
                    print(f"Error processing completed future: {e}")
                    failed_deployments += 1
            
            active_futures.append(deploy_config.submit(config))
        
        print(f"Waiting for {len(active_futures)} remaining simulations to complete...")
        for future in active_futures:
            try:
                result = future.result()
                if result:
                    simulation_results.append(result)
                else:
                    failed_deployments += 1
            except Exception as e:
                print(f"Error processing remaining future: {e}")
                failed_deployments += 1

        print(f"All simulations processed. Successful: {len(simulation_results)}, Failed: {failed_deployments}")
        print(f"Collected simulation data: {simulation_results}")

        if simulation_results:
            print("Skipping analysis steps for now.")
        else:
            print("No successful simulation data collected, skipping analysis.")
    else:
        print(f"No valid issues found or issue not verified. Result: {result}")

# Local debug run
if __name__ == "__main__":
    github_token = os.getenv("GITHUB_TOKEN")
    if not github_token:
        print("Error: GITHUB_TOKEN environment variable not set. Please create a .env file with your GitHub token.")
        exit(1)
    deployment_cron_job(repo_name="vacp2p/vaclab", github_token=github_token)
