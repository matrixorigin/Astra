#!/bin/bash
# MatrixOne entrypoint with logging

# Create logs directory if it doesn't exist
mkdir -p /mo-logs

# Start MatrixOne with logging to file (append mode to preserve history across restarts)
/mo-service -launch /etc/quickstart/launch.toml 2>&1 | tee -a /mo-logs/matrixone.log
