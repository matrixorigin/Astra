#!/bin/bash
# MatrixOne entrypoint with logging

# Create logs directory if it doesn't exist
mkdir -p /mo-logs

# Start MatrixOne with logging to file
/mo-service -launch /etc/quickstart/launch.toml 2>&1 | tee /mo-logs/matrixone.log
