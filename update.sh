#!/bin/bash
set -e

SERVICE_FILE="/etc/systemd/system/sink.service"

echo "Updating sink service..."

# Stop the service first
systemctl stop sink || true

# Rebuild
cargo build --release

# Install binary
sudo cp target/release/sink /usr/local/bin/sink
sudo chmod 755 /usr/local/bin/sink

# Update service file
sudo cp config/sink.service "$SERVICE_FILE"
sudo systemctl daemon-reload

# Start the service
systemctl start sink
echo "Service started"

# Show status
systemctl status sink --no-pager
