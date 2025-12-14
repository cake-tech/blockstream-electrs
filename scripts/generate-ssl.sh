#!/bin/bash
# Generate self-signed SSL certificates for development

set -e

SSL_DIR="nginx/ssl"
mkdir -p "$SSL_DIR"

echo "Generating self-signed SSL certificate..."
openssl req -x509 -nodes -days 365 -newkey rsa:2048 \
    -keyout "$SSL_DIR/key.pem" \
    -out "$SSL_DIR/cert.pem" \
    -subj "/C=US/ST=State/L=City/O=Organization/CN=localhost"

echo "✓ SSL certificates generated in $SSL_DIR/"
echo ""
echo "For production, use Let's Encrypt:"
echo "  certbot certonly --standalone -d yourdomain.com"
echo "  Then update nginx/conf.d/electrs.conf with the certificate paths"


