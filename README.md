# Blockstream Electrs - Complete Deployment Guide

A complete guide from repository setup to production deployment of Blockstream Electrs with Bitcoind, Nginx, and automatic GitHub deployments.

## 📋 Table of Contents

1. [Overview](#overview)
2. [Prerequisites](#prerequisites)
3. [Repository Setup](#repository-setup)
4. [Server Setup](#server-setup)
5. [Bitcoind Installation & Configuration](#bitcoind-installation--configuration)
6. [Electrs Deployment](#electrs-deployment)
7. [GitHub Actions Auto-Deployment](#github-actions-auto-deployment)
8. [Production Configuration](#production-configuration)
9. [Monitoring & Maintenance](#monitoring--maintenance)
10. [Troubleshooting](#troubleshooting)

---

## Overview

**What we're building:**
- **Bitcoin Core (bitcoind)** - Full Bitcoin node that stores the blockchain
- **Blockstream Electrs** - Indexing engine and HTTP API for blockchain queries
- **Nginx** - Reverse proxy with SSL termination and rate limiting
- **Docker** - Containerized deployment for easy management
- **GitHub Actions** - Automatic deployment on code push

**Architecture:**
```
┌─────────────────────────────────────────┐
│  Server                                 │
│                                         │
│  ┌──────────────────────────────────┐ │
│  │  bitcoind (systemd service)       │ │
│  │  - Port 8332 (RPC)                │ │
│  │  - /opt/bitcoin/ (data)            │ │
│  └──────────────┬─────────────────────┘ │
│                 │                       │
│                 │ RPC Connection        │
│                 │                       │
│  ┌──────────────▼─────────────────────┐ │
│  │  Docker Compose                     │ │
│  │  ┌──────────┐  ┌──────────────┐   │ │
│  │  │ nginx     │  │  electrs     │   │ │
│  │  │ (LB)      │  │  container   │   │ │
│  │  │ Port 80   │  │  Port 3000   │   │ │
│  │  │ Port 443  │  │  Port 50001  │   │ │
│  │  └────┬──────┘  └──────┬───────┘   │ │
│  │       └────────┬───────┘           │ │
│  └─────────────────┴───────────────────┘ │
└─────────────────────────────────────────┘
```

**How Electrs Connects to Bitcoind:**
- Bitcoind runs as a systemd service on the host
- Bitcoind binds to `0.0.0.0:8332` to accept Docker connections
- Electrs (in Docker) connects via RPC using Docker gateway IP (typically `172.17.0.1:8332`)
- Electrs reads RPC credentials from mounted `bitcoin.conf`
- Firewall restricts RPC access to localhost and Docker networks only

---

## Prerequisites

### Server Requirements
- **OS**: Ubuntu 20.04+ or Debian 11+ (Linux)
- **CPU**: 8+ cores (recommended)
- **RAM**: 32GB+ (16GB minimum)
- **Storage**: 1.5TB+ SSD (610GB+ for electrs DB, 500GB+ for bitcoind)
- **Network**: Stable internet connection
- **File Descriptors**: `ulimit -n 100000`

### Software Requirements
- Docker 20.10+
- Docker Compose 2.0+
- Git
- OpenSSL
- Root/sudo access

### GitHub Requirements
- GitHub account
- Repository (public or private)
- GitHub Actions enabled

---

## Repository Setup

### Step 1: Create GitHub Repository

1. Go to GitHub and create a new repository
2. Name it (e.g., `blockstream-electrs`)
3. Choose public or private
4. **Don't** initialize with README (we'll push existing code)

### Step 2: Clone and Push Your Code

```bash
# On your local machine
cd /path/to/blockstream-electrs

# Initialize git if not already done
git init
git add .
git commit -m "Initial commit: Blockstream Electrs deployment setup"

# Add your GitHub repository as remote
git remote add origin https://github.com/your-org/blockstream-electrs.git

# Push to GitHub
git branch -M main
git push -u origin main
```

### Step 3: Verify Repository Structure

Your repository should have:
```
blockstream-electrs/
├── .github/
│   └── workflows/
│       ├── rust.yml          # CI tests
│       ├── deploy-simple.yml # Auto-deployment
│       └── deploy.yml        # Alternative deployment
├── nginx/
│   ├── nginx.conf
│   └── conf.d/
│       └── electrs.conf
├── Dockerfile
├── docker-compose.yml
├── Cargo.toml
└── src/
```

---

## Server Setup

### Step 1: Connect to Server

```bash
ssh user@your-server-ip
```

### Step 2: Update System

```bash
sudo apt update && sudo apt upgrade -y
```

### Step 3: Install Docker and Docker Compose

```bash
# Install Docker
curl -fsSL https://get.docker.com -o get-docker.sh
sudo sh get-docker.sh

# Install Docker Compose
sudo curl -L "https://github.com/docker/compose/releases/latest/download/docker-compose-$(uname -s)-$(uname -m)" -o /usr/local/bin/docker-compose
sudo chmod +x /usr/local/bin/docker-compose

# Install Git
sudo apt install -y git

# Add user to docker group
sudo usermod -aG docker $USER
newgrp docker

# Verify installations
docker --version
docker-compose --version
git --version
```

### Step 4: Increase File Descriptor Limits

```bash
# Edit limits
echo "* soft nofile 100000" | sudo tee -a /etc/security/limits.conf
echo "* hard nofile 100000" | sudo tee -a /etc/security/limits.conf

# Edit sysctl
echo "fs.file-max = 100000" | sudo tee -a /etc/sysctl.conf
sudo sysctl -p

# Log out and back in for limits to take effect
exit
# Then SSH back in
```

### Step 5: Create Deployment Directory

```bash
sudo mkdir -p /opt/electrs
sudo chown -R $USER:$USER /opt/electrs
cd /opt/electrs
```

---

## Bitcoind Installation & Configuration

Bitcoind is the Bitcoin full node that electrs connects to. It must be installed and synced before electrs can index.

### Step 1: Install Bitcoin Core

```bash
# Download Bitcoin Core (latest stable version)
BITCOIN_VERSION="25.0"
cd /tmp

# Download
wget https://bitcoincore.org/bin/bitcoin-core-${BITCOIN_VERSION}/bitcoin-${BITCOIN_VERSION}-x86_64-linux-gnu.tar.gz

# Extract
tar -xzf bitcoin-${BITCOIN_VERSION}-x86_64-linux-gnu.tar.gz

# Install binaries
sudo install -m 0755 -o root -g root -t /usr/local/bin bitcoin-${BITCOIN_VERSION}/bin/*

# Clean up
rm -rf bitcoin-*

# Verify
bitcoind --version
bitcoin-cli --version
```

### Step 2: Create Bitcoind User and Directories

```bash
# Create dedicated user
sudo useradd -r -s /bin/false -d /opt/bitcoin bitcoind

# Create directories
sudo mkdir -p /opt/bitcoin/{data,blocks}
sudo chown -R bitcoind:bitcoind /opt/bitcoin
```

### Step 3: Configure Bitcoind

```bash
# Generate strong RPC password (SAVE THIS!)
RPC_PASSWORD=$(openssl rand -hex 32)
echo "=========================================="
echo "IMPORTANT: Save these credentials!"
echo "RPC User: bitcoinrpc"
echo "RPC Password: $RPC_PASSWORD"
echo "=========================================="

# Create bitcoin.conf
sudo tee /opt/bitcoin/data/bitcoin.conf > /dev/null <<EOF
# Network settings
server=1
rpcuser=bitcoinrpc
rpcpassword=${RPC_PASSWORD}
rpcbind=0.0.0.0
rpcallowip=127.0.0.1/8
rpcallowip=172.17.0.0/16
rpcport=8332

# Performance (adjust based on RAM)
dbcache=2048
maxconnections=40
maxmempool=512

# Indexing (electrs doesn't need txindex)
txindex=0

# ZMQ (for real-time updates)
zmqpubrawblock=tcp://0.0.0.0:28332
zmqpubrawtx=tcp://0.0.0.0:28333

# Data directories
datadir=/opt/bitcoin/data
blocksdir=/opt/bitcoin/blocks

# Logging
logtimestamps=1
logips=1
EOF

# Set permissions
sudo chown bitcoind:bitcoind /opt/bitcoin/data/bitcoin.conf
sudo chmod 600 /opt/bitcoin/data/bitcoin.conf
```

**⚠️ Security Note:** Bitcoind is now bound to `0.0.0.0` to allow Docker access. Ensure your firewall (UFW) blocks external access to port 8332. The `rpcallowip` settings restrict RPC access to localhost and Docker networks only.

### Step 4: Create Bitcoind Systemd Service

```bash
sudo tee /etc/systemd/system/bitcoind.service > /dev/null <<EOF
[Unit]
Description=Bitcoin daemon
After=network.target

[Service]
Type=simple
User=bitcoind
Group=bitcoind
ExecStart=/usr/local/bin/bitcoind -conf=/opt/bitcoin/data/bitcoin.conf
Restart=on-failure
RestartSec=60
TimeoutStopSec=600
PrivateTmp=true
NoNewPrivileges=true
ProtectSystem=full
ReadWritePaths=/opt/bitcoin
LimitNOFILE=65536

[Install]
WantedBy=multi-user.target
EOF

# Enable and start
sudo systemctl daemon-reload
sudo systemctl enable bitcoind
sudo systemctl start bitcoind

# Check status
sudo systemctl status bitcoind
```

### Step 5: Verify Bitcoind is Running

```bash
# Check status
sudo systemctl status bitcoind

# Test RPC connection
bitcoin-cli -conf=/opt/bitcoin/data/bitcoin.conf getblockchaininfo

# Monitor sync progress (takes days!)
watch -n 30 'bitcoin-cli -conf=/opt/electrs/db/bitcoin-data/bitcoin.conf getblockchaininfo | grep -E "blocks|verificationprogress"'
```

**⚠️ Important:** Bitcoind sync takes 1-2 days. Electrs can't index until bitcoind is synced, but you can continue with setup.

---

## Electrs Deployment

### Step 1: Clone Repository on Server

```bash
cd /opt/electrs

# Clone your repository
git clone https://github.com/your-org/blockstream-electrs.git .

# Or if using SSH
# git clone git@github.com:your-org/blockstream-electrs.git .
```

### Step 2: Create Required Directories

```bash
mkdir -p db logs bitcoin-data bitcoin-blocks
mkdir -p nginx/conf.d nginx/ssl nginx/logs
```

### Step 3: Generate SSL Certificates (Development)

```bash
# Generate self-signed certificates
openssl req -x509 -nodes -days 365 -newkey rsa:2048 \
    -keyout nginx/ssl/key.pem \
    -out nginx/ssl/cert.pem \
    -subj "/C=US/ST=State/L=City/O=Organization/CN=localhost"
```

**For Production:** Use Let's Encrypt (see Production Configuration section)

### Step 4: Configure Docker Compose for Bitcoind Connection

Edit `docker-compose.yml`:

```bash
nano docker-compose.yml
```

**Ensure volumes mount bitcoind directories:**
```yaml
volumes:
  - ./db:/data/db
  - ./logs:/data/logs
  - /opt/bitcoin/data:/bitcoin:ro          # Contains bitcoin.conf
  - /opt/bitcoin/blocks:/bitcoin-blocks:ro  # Blockchain data
```

**Verify command has correct RPC address:**
```yaml
command:
  - "--network=mainnet"
  - "--db-dir=/data/db"
  - "--daemon-dir=/bitcoin"                 # Reads bitcoin.conf from here
  - "--blocks-dir=/bitcoin-blocks"
  - "--daemon-rpc-addr=172.17.0.1:8332"    # Docker gateway IP (Linux)
```

**Note:** On Linux, `host.docker.internal` is unreliable. Use the Docker bridge gateway IP (typically `172.17.0.1`) instead. To find your Docker gateway IP:
```bash
docker network inspect bridge | grep Gateway
```

**Alternative (if gateway IP differs):** You can also use `network_mode: "host"` for the electrs service, but this requires adjusting nginx configuration.

### Step 5: Verify Bitcoind Connection

```bash
# Find Docker gateway IP (usually 172.17.0.1)
DOCKER_GATEWAY=$(docker network inspect bridge | grep Gateway | awk '{print $2}' | tr -d '"')

# Test connection from Docker
docker run --rm curlimages/curl:latest \
  curl --user bitcoinrpc:$(grep rpcpassword /opt/bitcoin/data/bitcoin.conf | cut -d= -f2) \
       --data-binary '{"jsonrpc":"1.0","id":"test","method":"getblockchaininfo","params":[]}' \
       http://${DOCKER_GATEWAY}:8332/
```

### Step 6: Build and Start Services

```bash
cd /opt/electrs

# Build Docker image
docker-compose build

# Start services
docker-compose up -d

# Check status
docker-compose ps

# View logs
docker-compose logs -f electrs
```

### Step 7: Verify Services

```bash
# Check containers
docker ps

# Test health endpoint
curl http://localhost/health

# Test API (may fail if bitcoind not synced)
curl http://localhost:3000/api/blocks/tip/height

# Test HTTPS
curl -k https://localhost/health
```

---

## GitHub Actions Auto-Deployment

### Step 1: Generate SSH Key for Deployment

**On your local machine:**

```bash
# Generate SSH key
ssh-keygen -t ed25519 -C "github-actions-deploy" -f ~/.ssh/github_deploy

# This creates:
# ~/.ssh/github_deploy (private key - keep secret!)
# ~/.ssh/github_deploy.pub (public key - add to server)
```

### Step 2: Add Public Key to Server

```bash
# Copy public key to server
ssh-copy-id -i ~/.ssh/github_deploy.pub user@your-server-ip

# Or manually
cat ~/.ssh/github_deploy.pub | ssh user@your-server-ip "mkdir -p ~/.ssh && cat >> ~/.ssh/authorized_keys"
```

**On server, verify:**
```bash
cat ~/.ssh/authorized_keys | grep github-actions-deploy
```

### Step 3: Add GitHub Secrets

1. Go to your GitHub repository
2. Click **Settings** → **Secrets and variables** → **Actions**
3. Click **New repository secret** and add:

   **DEPLOY_HOST**
   - Value: Your server IP or hostname (e.g., `192.168.1.100`)

   **DEPLOY_USER**
   - Value: Your SSH username (e.g., `ubuntu` or `deploy`)

   **DEPLOY_SSH_KEY**
   - Value: Entire content of `~/.ssh/github_deploy` (private key)
     ```bash
     cat ~/.ssh/github_deploy
     # Copy everything including -----BEGIN and -----END
     ```

   **DEPLOY_PORT** (optional)
   - Value: `22` (or your SSH port)

   **DEPLOY_PATH** (optional)
   - Value: `/opt/electrs` (or your deployment path)

### Step 4: Choose Deployment Workflow

You have two options in `.github/workflows/`:

- **`deploy-simple.yml`** (Recommended) - Server pulls code and rebuilds
- **`deploy.yml`** - Builds in CI and transfers image

Both trigger on push to `main`, `master`, or `new-index` branches.

**To disable one:**
```bash
mv .github/workflows/deploy.yml .github/workflows/deploy.yml.disabled
```

### Step 5: Test Deployment

```bash
# Make a test change locally
echo "# Test deployment" >> README.md
git add README.md
git commit -m "Test deployment"
git push origin main

# Check GitHub Actions tab - deployment should trigger automatically
```

### Step 6: Verify Deployment on Server

```bash
# SSH to server
ssh user@your-server-ip
cd /opt/electrs

# Check if containers restarted
docker-compose ps

# Check logs
docker-compose logs --tail=50 electrs
```

---

## Production Configuration

### SSL Certificates with Let's Encrypt

```bash
# Install certbot
sudo apt install -y certbot

# Stop nginx temporarily
cd /opt/electrs
docker-compose down

# Generate certificate (replace with your domain)
sudo certbot certonly --standalone -d electrs.yourdomain.com

# Certificates are in:
# /etc/letsencrypt/live/electrs.yourdomain.com/fullchain.pem
# /etc/letsencrypt/live/electrs.yourdomain.com/privkey.pem
```

**Update nginx config:**
```bash
nano nginx/conf.d/electrs.conf
```

Change SSL paths:
```nginx
ssl_certificate /etc/letsencrypt/live/electrs.yourdomain.com/fullchain.pem;
ssl_certificate_key /etc/letsencrypt/live/electrs.yourdomain.com/privkey.pem;
```

**Update docker-compose.yml to mount Let's Encrypt:**
```yaml
volumes:
  - /etc/letsencrypt:/etc/letsencrypt:ro
```

**Restart services:**
```bash
docker-compose up -d
```

### Firewall Configuration

```bash
# Configure UFW
sudo ufw default deny incoming
sudo ufw default allow outgoing

# Allow SSH (IMPORTANT - do this first!)
sudo ufw allow 22/tcp

# Allow HTTP/HTTPS
sudo ufw allow 80/tcp
sudo ufw allow 443/tcp

# IMPORTANT: Do NOT allow external access to Bitcoind RPC (port 8332)
# It's bound to 0.0.0.0 for Docker access, but should only be accessible locally
# The default deny incoming policy will block it, but verify:
sudo ufw status | grep 8332  # Should show nothing (blocked)

# Enable firewall
sudo ufw --force enable

# Check status
sudo ufw status
```

### Resource Limits

Edit `docker-compose.yml` to add resource limits:

```yaml
services:
  electrs:
    deploy:
      resources:
        limits:
          cpus: '8'
          memory: 16G
        reservations:
          cpus: '4'
          memory: 8G
```

### Log Rotation

```bash
sudo tee /etc/logrotate.d/electrs > /dev/null <<EOF
/opt/electrs/logs/*.log {
    daily
    rotate 7
    compress
    delaycompress
    missingok
    notifempty
    create 0644 $USER $USER
}
EOF
```

---

## Monitoring & Maintenance

### Monitoring Commands

```bash
# View real-time logs
docker-compose logs -f electrs

# Check container stats
docker stats

# Check disk usage
df -h
du -sh /opt/electrs/db/

# Check bitcoind sync status
bitcoin-cli -conf=/opt/bitcoin/data/bitcoin.conf getblockchaininfo

# Check service status
docker-compose ps
sudo systemctl status bitcoind
```

### Backup Script

```bash
cat > /opt/electrs/backup.sh <<'EOF'
#!/bin/bash
BACKUP_DIR="/opt/backups/electrs"
mkdir -p $BACKUP_DIR
DATE=$(date +%Y%m%d_%H%M%S)
tar -czf $BACKUP_DIR/electrs-db-$DATE.tar.gz -C /opt/electrs db/
# Keep only last 7 days
find $BACKUP_DIR -name "electrs-db-*.tar.gz" -mtime +7 -delete
EOF

chmod +x /opt/electrs/backup.sh

# Add to crontab (daily at 2 AM)
(crontab -l 2>/dev/null; echo "0 2 * * * /opt/electrs/backup.sh") | crontab -
```

### Common Maintenance Tasks

```bash
# Restart services
docker-compose restart

# Rebuild after code changes
docker-compose build electrs
docker-compose up -d electrs

# View logs
docker-compose logs -f electrs
docker-compose logs -f nginx

# Stop services
docker-compose down

# Start services
docker-compose up -d
```

---

## Troubleshooting

### Bitcoind Not Starting

```bash
# Check logs
sudo journalctl -u bitcoind -f

# Check permissions
sudo ls -la /opt/bitcoin/

# Check if port is in use
sudo netstat -tlnp | grep 8332

# Restart service
sudo systemctl restart bitcoind
```

### Electrs Can't Connect to Bitcoind

```bash
# Test bitcoind RPC
bitcoin-cli -conf=/opt/bitcoin/data/bitcoin.conf getblockchaininfo

# Test from Docker container (use Docker gateway IP)
DOCKER_GATEWAY=$(docker network inspect bridge | grep Gateway | awk '{print $2}' | tr -d '"')
docker exec electrs curl http://${DOCKER_GATEWAY}:8332

# Check docker-compose.yml has correct daemon-rpc-addr
grep "daemon-rpc-addr" docker-compose.yml

# Check bitcoind is listening
sudo netstat -tlnp | grep 8332
```

### Containers Keep Restarting

```bash
# Check logs for errors
docker-compose logs electrs

# Check resource usage
docker stats

# Check disk space
df -h

# Verify bitcoind is accessible
bitcoin-cli getblockchaininfo
```

### GitHub Actions Deployment Fails

```bash
# Test SSH connection manually
ssh -i ~/.ssh/github_deploy user@your-server-ip

# Check GitHub Actions logs for specific errors
# Verify all secrets are set correctly in GitHub

# Check server SSH logs
sudo tail -f /var/log/auth.log
```

### Port Conflicts

```bash
# Check what's using ports
sudo netstat -tlnp | grep -E "80|443|3000|50001"

# Change ports in docker-compose.yml if needed
# Update nginx config accordingly
```

### High Memory Usage

```bash
# Reduce cache size in docker-compose.yml
# Change --db-block-cache-mb=1024 to --db-block-cache-mb=512

# Restart with new settings
docker-compose up -d --force-recreate electrs
```

---

## Quick Reference

### Important Paths
- Bitcoind config: `/opt/bitcoin/data/bitcoin.conf`
- Bitcoind data: `/opt/bitcoin/data/`
- Electrs database: `/opt/electrs/db/`
- Electrs logs: `/opt/electrs/logs/`
- Deployment: `/opt/electrs/`

### Important Ports
- **8332** - Bitcoind RPC
- **80/443** - Nginx (HTTP/HTTPS)
- **3000** - Electrs HTTP API
- **50001** - Electrs Electrum RPC
- **4224** - Prometheus metrics

### Useful Commands

```bash
# Bitcoind
sudo systemctl status bitcoind
bitcoin-cli -conf=/opt/bitcoin/data/bitcoin.conf getblockchaininfo

# Electrs
docker-compose ps
docker-compose logs -f electrs
docker-compose restart electrs

# Deployment
git pull origin main
docker-compose build
docker-compose up -d
```

---

## Support

- Check logs: `docker-compose logs -f`
- Review GitHub Actions: Repository → Actions tab
- Check system resources: `htop`, `df -h`
- Review configuration files

---

## License

MIT
