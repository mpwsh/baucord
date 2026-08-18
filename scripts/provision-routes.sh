#!/bin/bash
# APISIX Provisioning Script
# This script provisions routes, services, stream routes, and global rules to APISIX
# It also handles certificate generation using certbot

# Exit on any command failure
set -e

# Send the exit code on script exit
trap 'echo "Exited with exit code $?."' EXIT

# Default values
APISIX_URL=${APISIX_URL:-"http://127.0.0.1:9180"}
APISIX_KEY=${APISIX_KEY:-""}
CURL_IGNORESSL=${CURL_IGNORESSL:-"false"}
CURL_QUIET=${CURL_QUIET:-"false"}
CURL_DEBUG=${CURL_DEBUG:-"false"}
YAML_CONFIG=${YAML_CONFIG:-"./apisix/routes.yml"}

# Curl options based on environment variables
ignoreSSL=$([ "$CURL_IGNORESSL" == "true" ] && echo "-k" || echo "")
showOutput=$([ "$CURL_QUIET" == "true" ] && echo "-o /dev/null" || echo "-o /tmp/debug_body")
debugCurl=$([ "$CURL_DEBUG" == "true" ] && echo "-v" || echo "")

echo "---------------------------------------"
echo "APISIX Provisioning Script"
echo "---------------------------------------"
echo "APISIX URL: $APISIX_URL"
echo "Config file: $YAML_CONFIG"

# Check if jq is installed
if ! command -v jq &>/dev/null; then
  echo "Error: jq is required but not installed. Please install jq first."
  exit 1
fi

# Check if yq is installed (for YAML parsing)
if ! command -v yq &>/dev/null; then
  echo "Error: yq is required but not installed. Please install yq first."
  exit 1
fi

# Function to validate HTTP response
validateHTTPResponse() {
  local status_code=$1
  if [[ $CURL_QUIET != "true" ]]; then
    echo "Response with code $status_code: $(cat /tmp/debug_body)"
  fi
  if [[ $status_code -ne 201 && $status_code -ne 200 ]]; then
    echo "ERROR: HTTP response is $status_code"
    return 1
  fi
  return 0
}

# Function to create/update a route
create_route() {
  local route_data=$1

  echo "Creating/updating route:"

  echo $route_data | jq
  status_code=$(curl -H "X-API-KEY: $APISIX_KEY" -s $ignoreSSL $debugCurl $showOutput -w "%{http_code}" \
    -X POST -d "$route_data" \
    "$APISIX_URL/apisix/admin/routes")

  validateHTTPResponse "$status_code"
}

# Function to create/update a service
create_service() {
  local service_data=$1
  local id=$(echo "$service_data" | jq -r '.id // "unknown"')
  echo $service_data | jq
  echo "Creating/updating service: $id"

  status_code=$(curl -H "X-API-KEY: $APISIX_KEY" -s $ignoreSSL $debugCurl $showOutput -w "%{http_code}" \
    -X PUT -d "$service_data" \
    "$APISIX_URL/apisix/admin/services/$id")

  validateHTTPResponse "$status_code"
}

# Function to create/update a stream route
create_stream_route() {
  local stream_route_data=$1
  local id=$(echo "$stream_route_data" | jq -r '.id // "unknown"')
  echo $stream_route_data | jq
  echo "Creating/updating stream route: $id"

  status_code=$(curl -H "X-API-KEY: $APISIX_KEY" -s $ignoreSSL $debugCurl $showOutput -w "%{http_code}" \
    -X PUT -d "$stream_route_data" \
    "$APISIX_URL/apisix/admin/stream_routes/${id}")

  validateHTTPResponse "$status_code"
}

# Function to create/update a global rule
create_global_rule() {
  local global_rule_data=$1
  local id=$(echo "$global_rule_data" | jq -r '.id // "unknown"')

  echo "Creating/updating global rule: $id"

  status_code=$(curl -H "X-API-KEY: $APISIX_KEY" -s $ignoreSSL $debugCurl $showOutput -w "%{http_code}" \
    -X PUT -d "$global_rule_data" \
    "$APISIX_URL/apisix/admin/global_rules/$id")

  validateHTTPResponse "$status_code"
}

# Main function to provision everything from YAML
provision_from_yaml() {
  local yaml_file=$1

  if [ ! -f "$yaml_file" ]; then
    echo "Error: YAML file '$yaml_file' not found"
    return 1
  fi

  echo "Provisioning from YAML file: $yaml_file"

  # Process services
  echo "Processing services..."
  local services_count=$(yq e '.services | length' "$yaml_file")
  if [ "$services_count" -gt 0 ]; then
    for i in $(seq 0 $((services_count - 1))); do
      local service_json=$(yq e ".services[$i]" -o=json "$yaml_file")
      create_service "$service_json"
    done
    echo "Processed $services_count services"
  else
    echo "No services found in YAML"
  fi

  # Process routes
  echo "Processing routes..."
  local routes_count=$(yq e '.routes | length' "$yaml_file")
  if [ "$routes_count" -gt 0 ]; then
    for i in $(seq 0 $((routes_count - 1))); do
      local route_json=$(yq e ".routes[$i]" -o=json "$yaml_file")
      create_route "$route_json"
    done
    echo "Processed $routes_count routes"
  else
    echo "No routes found in YAML"
  fi

  # Process stream routes
  echo "Processing stream routes..."
  local stream_routes_count=$(yq e '.stream_routes | length' "$yaml_file")
  if [ "$stream_routes_count" -gt 0 ]; then
    for i in $(seq 0 $((stream_routes_count - 1))); do
      local stream_route_json=$(yq e ".stream_routes[$i]" -o=json "$yaml_file")
      create_stream_route "$stream_route_json"
    done
    echo "Processed $stream_routes_count stream routes"
  else
    echo "No stream routes found in YAML"
  fi

  # Process global rules
  echo "Processing global rules..."
  local global_rules_count=$(yq e '.global_rules | length' "$yaml_file")
  if [ "$global_rules_count" -gt 0 ]; then
    for i in $(seq 0 $((global_rules_count - 1))); do
      local global_rule_json=$(yq e ".global_rules[$i]" -o=json "$yaml_file")
      create_global_rule "$global_rule_json"
    done
    echo "Processed $global_rules_count global rules"
  else
    echo "No global rules found in YAML"
  fi
}

provision_from_yaml "$YAML_CONFIG"

echo "---------------------------------------"
echo "APISIX provisioning completed successfully!"
echo "---------------------------------------"
