#!/bin/bash

# Default values for environment variables
HOST="${HOST:-127.0.0.1}"
PORT="${PORT:-3000}"
IP_SRC="${IP_SRC:-ConnectInfo}"
SEED_FILE="${SEED_FILE:-faucet.seed}"
SQLITE_FILE="${SQLITE_FILE:-faucet.sqlite}"
NETWORK="${NETWORK:-signet}"
ESPLORA="${ESPLORA:-https://explorer.bc-2.jp/api}"
L2_HTTP_ENDPOINT="${L2_HTTP_ENDPOINT:-https://ethereum-rpc.publicnode.com}"
SATS_PER_CLAIM="${SATS_PER_CLAIM:-1002000000}"
POW_DIFFICULTY="${POW_DIFFICULTY:-19}"



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
sats_per_claim = $SATS_PER_CLAIM
pow_difficulty = $POW_DIFFICULTY
EOL

# Debugging: Print the content of the generated faucet.toml
echo "Generated faucet.toml:"
cat /app/faucet.toml

# Run the application with the generated config
exec alpen-faucet --config /app/faucet.toml