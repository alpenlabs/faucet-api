#!/bin/bash

# Default values for environment variables
HOST="${HOST:-0.0.0.0}"
PORT="${PORT:-3000}"
IP_SRC="${IP_SRC:-ConnectInfo}"
SEED_FILE="${SEED_FILE:-faucet.seed}"
SQLITE_FILE="${SQLITE_FILE:-faucet.sqlite}"
NETWORK="${NETWORK:-signet}"
ESPLORA="${ESPLORA:-https://explorer.bc-2.jp/api}"
L2_HTTP_ENDPOINT="${L2_HTTP_ENDPOINT:-https://ethereum-rpc.publicnode.com}"
L1_SATS_PER_CLAIM="${L1_SATS_PER_CLAIM:-1_001_000_000}"
L2_SATS_PER_CLAIM="${L2_SATS_PER_CLAIM:-101_000_000}"

# Create the faucet.toml configuration file dynamically
cat <<EOL > /app/faucet.toml
host = "$HOST"
port = $PORT
ip_src = "$IP_SRC"
seed_file = "$SEED_FILE"
sqlite_file = "$SQLITE_FILE"
network = "$NETWORK"
esplora = "$ESPLORA"
l2_http_endpoint = "$L2_HTTP_ENDPOINT"
l1_sats_per_claim = $L1_SATS_PER_CLAIM
l2_sats_per_claim = $L2_SATS_PER_CLAIM
pow_difficulty = $POW_DIFFICULTY
EOL

# Debugging: Print the content of the generated faucet.toml
echo "Generated faucet.toml:"
cat /app/faucet.toml

# Run the application with the generated config
exec /usr/local/bin/alpen-faucet --config /app/faucet.toml
