#!/bin/bash
set -e

echo "=== Blockstream Electrs Deployment Script ==="
echo "This script sets up electrs with Docker Compose"
echo ""

# Check prerequisites
echo "[1/8] Checking prerequisites..."
if ! command -v docker &> /dev/null; then
    echo "ERROR: Docker is not installed. Please install Docker Desktop for macOS."
    exit 1
fi

if ! command -v docker-compose &> /dev/null && ! docker compose version &> /dev/null; then
    echo "ERROR: Docker Compose is not installed."
    exit 1
fi

# Use 'docker compose' if available, otherwise 'docker-compose'
if docker compose version &> /dev/null; then
    DOCKER_COMPOSE="docker compose"
else
    DOCKER_COMPOSE="docker-compose"
fi

echo "✓ Docker found"
echo "✓ Docker Compose found"

# Create directories
echo "[2/8] Creating directories..."
mkdir -p db logs bitcoin-data bitcoin-blocks
mkdir -p nginx/conf.d nginx/ssl nginx/logs
echo "✓ Directories created"

# Generate SSL certificates (self-signed for development)
echo "[3/8] Generating SSL certificates..."
if [ ! -f nginx/ssl/cert.pem ] || [ ! -f nginx/ssl/key.pem ]; then
    openssl req -x509 -nodes -days 365 -newkey rsa:2048 \
        -keyout nginx/ssl/key.pem \
        -out nginx/ssl/cert.pem \
        -subj "/C=US/ST=State/L=City/O=Organization/CN=localhost" 2>/dev/null
    echo "✓ SSL certificates generated"
else
    echo "✓ SSL certificates already exist"
fi

# Check bitcoind configuration
echo "[4/8] Checking bitcoind configuration..."
BITCOIN_DIR="${HOME}/.bitcoin"
if [ -d "$BITCOIN_DIR" ]; then
    echo "✓ Found bitcoind directory at $BITCOIN_DIR"
    echo "  Note: Make sure bitcoind is running and accessible at 127.0.0.1:8332"
    echo "  You may need to update docker-compose.yml to mount your bitcoind directory"
    echo ""
    read -p "Do you want to update docker-compose.yml to use $BITCOIN_DIR? (y/n) " -n 1 -r
    echo
    if [[ $REPLY =~ ^[Yy]$ ]]; then
        # Update docker-compose.yml to use the actual bitcoind directory
        if [[ "$OSTYPE" == "darwin"* ]]; then
            # macOS - use direct path mounting
            sed -i '' "s|./bitcoin-data:/bitcoin:ro|$BITCOIN_DIR:/bitcoin:ro|g" docker-compose.yml
            # Try to find blocks directory
            if [ -d "$BITCOIN_DIR/blocks" ]; then
                sed -i '' "s|./bitcoin-blocks:/bitcoin-blocks:ro|$BITCOIN_DIR/blocks:/bitcoin-blocks:ro|g" docker-compose.yml
            fi
        else
            sed -i "s|./bitcoin-data:/bitcoin:ro|$BITCOIN_DIR:/bitcoin:ro|g" docker-compose.yml
            if [ -d "$BITCOIN_DIR/blocks" ]; then
                sed -i "s|./bitcoin-blocks:/bitcoin-blocks:ro|$BITCOIN_DIR/blocks:/bitcoin-blocks:ro|g" docker-compose.yml
            fi
        fi
        echo "✓ Updated docker-compose.yml"
    fi
else
    echo "⚠ Bitcoind directory not found at $BITCOIN_DIR"
    echo "  You'll need to:"
    echo "  1. Install and configure bitcoind"
    echo "  2. Update docker-compose.yml to point to your bitcoind directory"
    echo "  3. Ensure bitcoind is running and accessible at 127.0.0.1:8332"
fi

# Build Docker image
echo "[5/8] Building Docker image..."
$DOCKER_COMPOSE build
echo "✓ Docker image built"

# Check if containers are already running
echo "[6/8] Checking existing containers..."
if $DOCKER_COMPOSE ps | grep -q "electrs\|nginx"; then
    echo "⚠ Containers are already running"
    read -p "Do you want to restart them? (y/n) " -n 1 -r
    echo
    if [[ $REPLY =~ ^[Yy]$ ]]; then
        $DOCKER_COMPOSE down
        echo "✓ Stopped existing containers"
    else
        echo "Skipping container start. Use 'docker-compose up -d' to start manually."
        exit 0
    fi
fi

# Start services
echo "[7/8] Starting services..."
$DOCKER_COMPOSE up -d
echo "✓ Services started"

# Wait for services to be ready
echo "[8/8] Waiting for services to initialize..."
sleep 5

# Verify deployment
echo ""
echo "=== Deployment Complete ==="
echo ""
echo "Service Status:"
$DOCKER_COMPOSE ps
echo ""
echo "=== Testing Endpoints ==="
echo "Health check:"
curl -s http://localhost/health 2>/dev/null || echo "  Health check failed (may need more time)"
echo ""
echo "API test (may fail if bitcoind is not synced):"
curl -s http://localhost:3000/api/blocks/tip/height 2>/dev/null || echo "  API not ready yet (wait for bitcoind sync)"
echo ""
echo "=== Useful Commands ==="
echo "View logs:              $DOCKER_COMPOSE logs -f"
echo "View electrs logs:      $DOCKER_COMPOSE logs -f electrs"
echo "View nginx logs:        $DOCKER_COMPOSE logs -f nginx"
echo "Stop services:          $DOCKER_COMPOSE down"
echo "Restart services:       $DOCKER_COMPOSE restart"
echo "Rebuild and restart:    $DOCKER_COMPOSE up -d --build"
echo ""
echo "=== Important Notes ==="
echo "1. Make sure bitcoind is running: bitcoin-cli getblockchaininfo"
echo "2. Wait for bitcoind to sync before electrs can index"
echo "3. Monitor electrs logs: $DOCKER_COMPOSE logs -f electrs"
echo "4. For production, replace self-signed SSL with Let's Encrypt"
echo "5. Database location: ./db/"
echo "6. Logs location: ./logs/"
echo ""
echo "=== Next Steps ==="
echo "1. Ensure bitcoind is running and synced"
echo "2. Monitor electrs logs for indexing progress"
echo "3. Test API endpoints once indexing is complete"
echo ""


