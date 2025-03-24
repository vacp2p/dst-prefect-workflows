#!/bin/bash

# Default values
NAMESPACE="zerotesting"
CLEAN_ALL=false

# Function to display usage information
usage() {
    echo "Usage: $0 [-n NAMESPACE] [-a|--all]"
    echo "  -n NAMESPACE    Specify the namespace to clean (default: zerotesting)"
    echo "  -a, --all       Clean all dst-datestamp Helm charts, regardless of date"
    exit 1
}

# Parse command line arguments
while [[ $# -gt 0 ]]; do
    key="$1"
    case $key in
        -n)
        NAMESPACE="$2"
        shift # past argument
        shift # past value
        ;;
        -a|--all)
        CLEAN_ALL=true
        shift # past argument
        ;;
        *)
        usage
        ;;
    esac
done

echo "Cleaning up Helm releases in namespace: $NAMESPACE"

# Get today's date in YYYYMMDD format
TODAY=$(date +%Y%m%d)

# Function to uninstall a Helm release
uninstall_release() {
    release=$1
    echo "Uninstalling release: $release"
    helm uninstall $release -n $NAMESPACE
}

# Get all Helm releases in the specified namespace
releases=$(helm list -n $NAMESPACE -q)

for release in $releases; do
    # All starting with dst-
    #if [[ $release == dst-* ]]; then
    # All starting with dst-20241020-* (or any other date)
    if [[ $release =~ ^dst-[0-9]{8}-[0-9]+$ ]]; then
        if [ "$CLEAN_ALL" = true ]; then
            uninstall_release $release
        else
            # Extract the date from the release name (assuming format dst-YYYYMMDD-HHMM)
            release_date=$(echo $release | cut -d'-' -f2)
            
            if [ "$release_date" = "$TODAY" ]; then
                uninstall_release $release
            fi
        fi
    fi
done

echo "Cleanup completed."
