## Requirements
* Kubectl installed
* kubeconfig pointed at our Kubernetes cluster
* Public GitHub token (no privileges needed)

## How to Use

0.  **Clone the Repo:**
    First, clone the repositry from GitHub using Git.

    ```bash
    git clone https://github.com/vacp2p/dst-prefect-workflows.git
    ```

1.  **Install Dependencies:**
    Install the required Python packages, including Prefect. It's recommended to use a virtual environment.

    You'll need python3-pip installed.

    ```bash
    pip install -r requirements.txt
    # If you encounter system package issues, you might try:
    # pip install -U prefect --break-system-packages
    ```

2.  **Configure Environment:**
    Create a `.env` file in the `prefect/` directory and add your GitHub Personal Access Token with repository access:
    ```dotenv
    GITHUB_TOKEN=ghp_YOUR_GITHUB_TOKEN
    ```

3.  **Prepare GitHub Issue:**
    *   Create a GitHub issue in the target repository.
    *   Fill in the issue body with the simulation parameters according to the template expected by `run.py` (e.g., program type, node count, duration, docker image, etc.).
    *   Add the `needs-scheduling` label to the issue. Ensure this label is added by an authorized user (defined in `AUTHORIZED_USERS` within `run.py`).

4.  **Run the Prefect Flow:**
    Execute the `run.py` script. This will start the Prefect flow, which will scan the configured GitHub repository for issues labeled `needs-scheduling`.
    ```bash
    python run.py
    ```
    The flow will:
    *   Find valid issues created by authorized users.
    *   Parse the issue body to generate simulation configurations.
    *   Deploy the simulations using Helm based on the configurations.
    *   Cleanup the simulations after they have been running for the configured duration.
    *   (In the future) update the issue label to `simulation-done` upon completion.

5.  **Collect Results:**
    Simulation results and logs might be stored in the `test/` directory or other locations depending on the specific Helm chart and simulation setup.

6.  **Post-Analysis:**
    The run.py script will also generate a summary of the simulation results and save graphs in the main folder and results in the "test" folder.

