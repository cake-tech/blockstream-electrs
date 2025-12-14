.PHONY: help build up down restart logs clean ssl test

help: ## Show this help message
	@echo "Blockstream Electrs - Deployment Commands"
	@echo ""
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) | sort | awk 'BEGIN {FS = ":.*?## "}; {printf "\033[36m%-20s\033[0m %s\n", $$1, $$2}'

build: ## Build Docker images
	docker-compose build

up: ## Start services
	docker-compose up -d

down: ## Stop services
	docker-compose down

restart: ## Restart services
	docker-compose restart

logs: ## View logs (follow mode)
	docker-compose logs -f

logs-electrs: ## View electrs logs
	docker-compose logs -f electrs

logs-nginx: ## View nginx logs
	docker-compose logs -f nginx

ssl: ## Generate SSL certificates
	./scripts/generate-ssl.sh

clean: ## Remove containers and volumes (WARNING: deletes data)
	docker-compose down -v
	rm -rf db/* logs/* nginx/logs/*

test: ## Test endpoints
	@echo "Testing health endpoint..."
	@curl -s http://localhost/health || echo "Health check failed"
	@echo ""
	@echo "Testing API endpoint..."
	@curl -s http://localhost:3000/api/blocks/tip/height || echo "API not ready"

status: ## Show container status
	docker-compose ps

rebuild: ## Rebuild and restart electrs
	docker-compose build electrs
	docker-compose up -d electrs

deploy: ## Run full deployment script
	./deploy.sh


