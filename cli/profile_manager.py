"""Profile management utilities."""

import json
from pathlib import Path


class ProfileManager:
    """Manage user profiles in credentials file."""

    def __init__(self, credentials_path: Path | None = None):
        self.credentials_path = credentials_path or Path.home() / ".mo-agent" / "credentials.json"

    def _load_data(self) -> dict:
        """Load credentials file."""
        if not self.credentials_path.exists():
            return {"current_profile": "default", "profiles": {}}
        try:
            return json.loads(self.credentials_path.read_text())
        except Exception:
            return {"current_profile": "default", "profiles": {}}

    def _save_data(self, data: dict) -> None:
        """Save credentials file."""
        self.credentials_path.parent.mkdir(parents=True, exist_ok=True)
        self.credentials_path.write_text(json.dumps(data, indent=2))
        self.credentials_path.chmod(0o600)

    def list_profiles(self) -> list[dict]:
        """List all profiles."""
        data = self._load_data()
        current = data.get("current_profile", "default")
        profiles = []
        for name, profile in data.get("profiles", {}).items():
            profiles.append(
                {
                    "name": name,
                    "username": profile.get("username", "unknown"),
                    "current": name == current,
                }
            )
        return profiles

    def get_current_profile(self) -> str | None:
        """Get current profile name."""
        data = self._load_data()
        return data.get("current_profile")

    def set_current_profile(self, profile_name: str) -> None:
        """Set current profile."""
        data = self._load_data()
        if profile_name not in data.get("profiles", {}):
            raise ValueError(f"Profile '{profile_name}' not found")
        data["current_profile"] = profile_name
        self._save_data(data)

    def delete_profile(self, profile_name: str) -> None:
        """Delete a profile."""
        data = self._load_data()
        if profile_name not in data.get("profiles", {}):
            raise ValueError(f"Profile '{profile_name}' not found")
        del data["profiles"][profile_name]
        # If deleting current profile, switch to another or default
        if data.get("current_profile") == profile_name:
            remaining = list(data["profiles"].keys())
            data["current_profile"] = remaining[0] if remaining else "default"
        self._save_data(data)
