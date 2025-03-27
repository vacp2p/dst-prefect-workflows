# Python Imports

# Project Imports
import src.logger.logger
from src.mesh_analysis.waku_message_log_analyzer import WakuMessageLogAnalyzer


if __name__ == '__main__':
    # Timestamp of the simulation   
    timestamp = "[2025-03-27 12:14:47, 2025-03-27 12:15:55]"
    stateful_sets = ["bootstrap", "nodes"] if "waku" == "waku" else ["nodes"]
    # Example of data analysis from cluster
    log_analyzer = WakuMessageLogAnalyzer(stateful_sets, timestamp, dump_analysis_dir='local_data/waku-300x-1m/')
    # Example of data analysis from local
    # log_analyzer = WakuMessageLogAnalyzer(local_folder_to_analyze='lpt_duptest_debug', dump_analysis_dir='lpt_duptest_debug/notion')

    log_analyzer.analyze_message_logs(True)
    log_analyzer.check_store_messages()
    log_analyzer.analyze_message_timestamps(time_difference_threshold=2)
