"""Base for skill table definitions (BYOD tables)."""

from sqlalchemy.orm import DeclarativeBase


class SkillTableBase(DeclarativeBase):
    """Declarative base for skill tables created in user BYOD databases.

    Separate from platform Base — these tables are created via SkillManager.install()
    on the user's own database, not the platform database.
    """
    pass
