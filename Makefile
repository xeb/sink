.PHONY: build install update uninstall clean status logs restart stop start

BINARY_NAME = sink
INSTALL_PATH = /usr/local/bin/$(BINARY_NAME)
CONFIG_DIR = /etc/sink
DATA_DIR = /var/lib/sink
SERVICE_FILE = /etc/systemd/system/$(BINARY_NAME).service

build:
	cargo build --release

install: build
	@echo "Installing $(BINARY_NAME)..."
	sudo mkdir -p $(CONFIG_DIR)
	sudo mkdir -p $(DATA_DIR)
	sudo chown $$USER:$$USER $(DATA_DIR)
	sudo cp target/release/$(BINARY_NAME) $(INSTALL_PATH)
	sudo chmod 755 $(INSTALL_PATH)
	@if [ ! -f $(CONFIG_DIR)/config.toml ]; then \
		sudo cp config.example.toml $(CONFIG_DIR)/config.toml; \
		sudo chown $$USER:$$USER $(CONFIG_DIR)/config.toml; \
		sudo chmod 600 $(CONFIG_DIR)/config.toml; \
		echo "Installed default config to $(CONFIG_DIR)/config.toml"; \
	else \
		echo "Config already exists at $(CONFIG_DIR)/config.toml (not overwriting)"; \
	fi
	sudo cp config/sink.service $(SERVICE_FILE)
	sudo systemctl daemon-reload
	sudo systemctl enable $(BINARY_NAME)
	sudo systemctl start $(BINARY_NAME)
	@echo "$(BINARY_NAME) installed and started"
	@echo "Check status with: make status"
	@echo "View logs with: make logs"

update: build
	@echo "Updating $(BINARY_NAME)..."
	sudo systemctl stop $(BINARY_NAME) || true
	sudo cp target/release/$(BINARY_NAME) $(INSTALL_PATH)
	sudo chmod 755 $(INSTALL_PATH)
	sudo cp config/sink.service $(SERVICE_FILE)
	sudo systemctl daemon-reload
	sudo systemctl start $(BINARY_NAME)
	@echo "$(BINARY_NAME) updated and restarted"

uninstall:
	@echo "Uninstalling $(BINARY_NAME)..."
	sudo systemctl stop $(BINARY_NAME) || true
	sudo systemctl disable $(BINARY_NAME) || true
	sudo rm -f $(SERVICE_FILE)
	sudo rm -f $(INSTALL_PATH)
	sudo systemctl daemon-reload
	@echo "$(BINARY_NAME) uninstalled"
	@echo "Note: Config and data preserved in $(CONFIG_DIR) and $(DATA_DIR)"
	@echo "To remove all data: sudo rm -rf $(CONFIG_DIR) $(DATA_DIR)"

clean:
	cargo clean

status:
	sudo systemctl status $(BINARY_NAME)

logs:
	sudo journalctl -u $(BINARY_NAME) -f

restart:
	sudo systemctl restart $(BINARY_NAME)

stop:
	sudo systemctl stop $(BINARY_NAME)

start:
	sudo systemctl start $(BINARY_NAME)
