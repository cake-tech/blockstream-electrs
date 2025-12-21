#!/bin/bash
# Script to verify Docker network configuration for electrs

echo "=== DOCKER NETWORK DIAGNOSTIC ==="
echo ""

# Check if electrs network exists
echo "1. Checking electrs-network..."
docker network inspect electrs-network > /dev/null 2>&1
if [ $? -eq 0 ]; then
    echo "   ✓ electrs-network exists"
    
    # Get gateway IP
    GATEWAY_IP=$(docker network inspect electrs-network | grep -i gateway | head -1 | awk '{print $2}' | tr -d '",')
    echo "   Gateway IP: $GATEWAY_IP"
    
    # Check what's configured in docker-compose.yml
    CONFIGURED_IP=$(grep "daemon-rpc-addr" docker-compose.yml | grep -oP '\d+\.\d+\.\d+\.\d+' | head -1)
    echo "   Configured IP in docker-compose.yml: $CONFIGURED_IP"
    
    if [ "$GATEWAY_IP" != "$CONFIGURED_IP" ]; then
        echo "   ⚠ WARNING: Gateway IP mismatch!"
        echo "   The configured IP ($CONFIGURED_IP) doesn't match the actual gateway ($GATEWAY_IP)"
        echo "   This will cause connection failures!"
        echo ""
        echo "   Fix: Update docker-compose.yml:"
        echo "   Change: --daemon-rpc-addr=$CONFIGURED_IP:8332"
        echo "   To:     --daemon-rpc-addr=$GATEWAY_IP:8332"
    else
        echo "   ✓ Gateway IP matches configuration"
    fi
else
    echo "   ✗ electrs-network does not exist"
    echo "   Run: docker-compose up -d (this will create the network)"
fi

echo ""
echo "2. Testing bitcoind connection from host..."
BITCOIND_RESPONSE=$(curl -s --user bitcoinrpc:cab44cb7d0ee18fe1c556ae50bae373c8ea81212179c09cfbdee9425dfcbd516 --data-binary '{"jsonrpc":"1.0","id":"test","method":"getblockchaininfo","params":[]}' -H 'content-type: text/plain;' --max-time 5 http://localhost:8332/ 2>&1)
if echo "$BITCOIND_RESPONSE" | grep -q "result"; then
    echo "   ✓ Bitcoind RPC is accessible from host"
else
    echo "   ✗ Bitcoind RPC is NOT accessible from host"
    echo "   Response: $BITCOIND_RESPONSE"
fi

echo ""
echo "3. Testing bitcoind connection from Docker network..."
if [ ! -z "$GATEWAY_IP" ]; then
    DOCKER_TEST=$(docker run --rm --network electrs-network curlimages/curl:latest curl -s --max-time 5 --user bitcoinrpc:cab44cb7d0ee18fe1c556ae50bae373c8ea81212179c09cfbdee9425dfcbd516 --data-binary '{"jsonrpc":"1.0","id":"test","method":"getblockchaininfo","params":[]}' -H 'content-type: text/plain;' http://$GATEWAY_IP:8332/ 2>&1)
    if echo "$DOCKER_TEST" | grep -q "result"; then
        echo "   ✓ Bitcoind RPC is accessible from Docker network at $GATEWAY_IP:8332"
    else
        echo "   ✗ Bitcoind RPC is NOT accessible from Docker network at $GATEWAY_IP:8332"
        echo "   This is the problem! Check bitcoind firewall/rpcallowip settings"
    fi
fi

echo ""
echo "4. Checking electrs container network..."
ELECTRS_IP=$(docker inspect electrs --format '{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}' 2>/dev/null)
if [ ! -z "$ELECTRS_IP" ]; then
    echo "   Electrs container IP: $ELECTRS_IP"
    echo "   ✓ Electrs container is running"
else
    echo "   ✗ Electrs container is not running"
fi

echo ""
echo "5. Checking for connection issues..."
CONNECTION_COUNT=$(ss -tnp | grep :8332 | grep ESTAB | wc -l)
FIN_WAIT=$(ss -tnp | grep :8332 | grep FIN-WAIT | wc -l)
echo "   Established connections to bitcoind: $CONNECTION_COUNT"
echo "   Connections in FIN-WAIT state: $FIN_WAIT"
if [ "$FIN_WAIT" -gt 10 ]; then
    echo "   ⚠ WARNING: Many connections in FIN-WAIT state - connection cleanup issues"
fi

echo ""
echo "=== SUMMARY ==="
if [ "$GATEWAY_IP" != "$CONFIGURED_IP" ] && [ ! -z "$GATEWAY_IP" ] && [ ! -z "$CONFIGURED_IP" ]; then
    echo "⚠ ACTION REQUIRED: Update docker-compose.yml with correct gateway IP: $GATEWAY_IP"
    echo ""
    echo "Run this command to fix:"
    echo "sed -i 's|--daemon-rpc-addr=$CONFIGURED_IP:8332|--daemon-rpc-addr=$GATEWAY_IP:8332|g' docker-compose.yml"
else
    echo "✓ Network configuration looks correct"
fi

