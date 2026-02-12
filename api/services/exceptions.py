"""Custom exceptions for services"""


class ResourceNotFoundError(Exception):
    """Raised when a resource is not found"""
    pass


class PermissionDeniedError(Exception):
    """Raised when access is denied"""
    pass
