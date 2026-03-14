"""Startup configuration validation for critical services.

This module validates that all required configuration is present and correct
before the application starts, preventing runtime failures.
"""

import os
import logging
from typing import List, Tuple

import httpx

logger = logging.getLogger(__name__)


class ConfigValidationError(Exception):
    """Raised when critical configuration is missing or invalid."""
    pass


def validate_memoria_config() -> List[str]:
    """Validate Memoria configuration.
    
    Returns:
        List of validation errors (empty if all valid)
    """
    errors = []
    
    # Check required environment variables
    memoria_url = os.environ.get("MEMORIA_BASE_URL")
    memoria_master_key = os.environ.get("MEMORIA_MASTER_KEY")
    memoria_api_key = os.environ.get("MEMORIA_API_KEY")
    
    if not memoria_url:
        errors.append("MEMORIA_BASE_URL is required but not set")
    elif not memoria_url.startswith(("http://", "https://")):
        errors.append(f"MEMORIA_BASE_URL must be HTTP URL, got: {memoria_url}")
    
    if not memoria_master_key and not memoria_api_key:
        errors.append("Either MEMORIA_MASTER_KEY or MEMORIA_API_KEY is required")
    
    if memoria_master_key and len(memoria_master_key.strip()) < 8:
        errors.append("MEMORIA_MASTER_KEY is too short (minimum 8 characters)")
    
    if memoria_api_key and not memoria_api_key.startswith("sk-"):
        errors.append("MEMORIA_API_KEY should start with 'sk-'")
    
    return errors


def validate_memoria_connectivity() -> List[str]:
    """Validate that Memoria service is reachable and healthy.
    
    Returns:
        List of connectivity errors (empty if all good)
    """
    errors = []
    
    memoria_url = os.environ.get("MEMORIA_BASE_URL")
    if not memoria_url:
        return ["MEMORIA_BASE_URL not set, skipping connectivity check"]
    
    try:
        # Test health endpoint
        response = httpx.get(f"{memoria_url}/health", timeout=10.0)
        
        if response.status_code != 200:
            errors.append(f"Memoria health check failed: HTTP {response.status_code}")
            return errors
        
        health = response.json()
        if health.get("status") not in ["ok", "healthy"]:
            errors.append(f"Memoria service unhealthy: {health.get('status')}")
        
        if health.get("database") != "connected":
            errors.append(f"Memoria database not connected: {health.get('database')}")
        
        # Test auth
        auth_key = os.environ.get("MEMORIA_MASTER_KEY") or os.environ.get("MEMORIA_API_KEY")
        if auth_key:
            auth_response = httpx.post(
                f"{memoria_url}/v1/memories/retrieve",
                json={"user_id": "config-test", "query": "test", "top_k": 1},
                headers={"Authorization": f"Bearer {auth_key}"},
                timeout=10.0
            )
            
            if auth_response.status_code in [401, 403]:
                errors.append(f"Memoria authentication failed: invalid key")
            elif auth_response.status_code >= 500:
                errors.append(f"Memoria server error: HTTP {auth_response.status_code}")
        
    except httpx.TimeoutException:
        errors.append(f"Memoria service timeout at {memoria_url}")
    except httpx.RequestError as e:
        errors.append(f"Cannot reach Memoria at {memoria_url}: {e}")
    except Exception as e:
        errors.append(f"Memoria connectivity check failed: {e}")
    
    return errors


def validate_all_startup_config() -> Tuple[List[str], List[str]]:
    """Validate all critical startup configuration.
    
    Returns:
        Tuple of (errors, warnings)
    """
    errors = []
    warnings = []
    
    # Validate Memoria
    memoria_errors = validate_memoria_config()
    errors.extend(memoria_errors)
    
    # Only test connectivity if basic config is valid
    if not memoria_errors:
        connectivity_errors = validate_memoria_connectivity()
        # Connectivity issues are warnings, not hard errors
        warnings.extend(connectivity_errors)
    
    # Add other critical config validation here
    # e.g., database, LLM providers, etc.
    
    return errors, warnings


def check_startup_config(fail_on_warnings: bool = False) -> None:
    """Check startup configuration and log results.
    
    Args:
        fail_on_warnings: If True, treat warnings as errors
        
    Raises:
        ConfigValidationError: If critical configuration is invalid
    """
    logger.info("Validating startup configuration...")
    
    errors, warnings = validate_all_startup_config()
    
    if warnings:
        for warning in warnings:
            logger.warning(f"Config warning: {warning}")
    
    if errors:
        for error in errors:
            logger.error(f"Config error: {error}")
        
        error_summary = f"Found {len(errors)} configuration error(s)"
        if warnings:
            error_summary += f" and {len(warnings)} warning(s)"
        
        raise ConfigValidationError(error_summary)
    
    if warnings and fail_on_warnings:
        warning_summary = f"Found {len(warnings)} configuration warning(s) (treated as errors)"
        raise ConfigValidationError(warning_summary)
    
    logger.info("✅ Startup configuration validation passed")


if __name__ == "__main__":
    # Allow running as script for manual validation
    import sys
    
    # Load .env file if it exists
    try:
        from dotenv import load_dotenv
        load_dotenv()
    except ImportError:
        pass  # dotenv not available, skip
    
    try:
        check_startup_config(fail_on_warnings="--strict" in sys.argv)
        print("✅ All configuration valid")
    except ConfigValidationError as e:
        print(f"❌ Configuration validation failed: {e}")
        sys.exit(1)
