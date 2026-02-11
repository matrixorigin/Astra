"""Multi-repository management example."""

from core.repos import AccessScope, OwnerType, RepoRegistry, RepoType
from sdk import Database

# Initialize
db = Database()
registry = RepoRegistry(db=db)

print("=" * 60)
print("Multi-Repository Management Example")
print("=" * 60)

# 1. Register MatrixOne code repository
print("\n1. Register MatrixOne code repository")
matrixone_repo = registry.create(
    repo_url="https://github.com/matrixorigin/matrixone",
    repo_type=RepoType.CODE,
    owner_id="team_matrixone",
    owner_type=OwnerType.TENANT,
    access_scope=AccessScope.WRITE,
    repo_group="matrixone-project",
    metadata={
        "default_branch": "main",
        "language": "go",
        "description": "MatrixOne main repository",
    },
)
print(f"✓ Created: {matrixone_repo.repo_url}")
print(f"  Type: {matrixone_repo.repo_type.value}")
print(f"  Group: {matrixone_repo.repo_group}")
print(f"  Access: {matrixone_repo.access_scope.value}")

# 2. Register CI repository
print("\n2. Register CI repository")
ci_repo = registry.create(
    repo_url="https://github.com/matrixorigin/mo-ci",
    repo_type=RepoType.CI,
    owner_id="team_matrixone",
    owner_type=OwnerType.TENANT,
    access_scope=AccessScope.READ,
    repo_group="matrixone-project",
    metadata={
        "default_branch": "main",
        "ci_paths": [".github/workflows"],
    },
)
print(f"✓ Created: {ci_repo.repo_url}")

# 3. Register tester repository
print("\n3. Register tester repository")
tester_repo = registry.create(
    repo_url="https://github.com/matrixorigin/mo-tester",
    repo_type=RepoType.TESTER,
    owner_id="team_matrixone",
    owner_type=OwnerType.TENANT,
    access_scope=AccessScope.ADMIN,
    repo_group="matrixone-project",
    metadata={
        "default_branch": "main",
        "test_paths": ["cases/"],
    },
)
print(f"✓ Created: {tester_repo.repo_url}")

# 4. List all repositories for the team
print("\n4. List all repositories for team_matrixone")
repos = registry.list_by_owner("team_matrixone")
print(f"Total repositories: {len(repos)}")
for repo in repos:
    print(f"  - {repo.repo_url} ({repo.repo_type.value})")

# 5. Filter by repository type
print("\n5. Filter by repository type (CODE)")
code_repos = registry.list_by_owner("team_matrixone", repo_type=RepoType.CODE)
print(f"Code repositories: {len(code_repos)}")
for repo in code_repos:
    print(f"  - {repo.repo_url}")

# 6. Update repository metadata
print("\n6. Update repository metadata")
registry.update_metadata(
    matrixone_repo.repo_id,
    {
        "default_branch": "main",
        "language": "go",
        "description": "MatrixOne - Hyperconverged cloud-native database",
        "stars": 1800,
    },
)
updated_repo = registry.get(matrixone_repo.repo_id)
print(f"✓ Updated metadata: {updated_repo.metadata}")

# 7. Get repository by URL
print("\n7. Get repository by URL")
found_repo = registry.get_by_url("https://github.com/matrixorigin/matrixone", "team_matrixone")
if found_repo:
    print(f"✓ Found: {found_repo.repo_url}")
    print(f"  Owner: {found_repo.owner_id}")
    print(f"  Type: {found_repo.repo_type.value}")

# 8. List repositories by group
print("\n8. List repositories by group (matrixone-project)")
group_repos = registry.list_by_group("matrixone-project")
print(f"Repositories in group: {len(group_repos)}")
for repo in group_repos:
    print(f"  - {repo.repo_url} ({repo.repo_type.value})")

# Cleanup
print("\n9. Cleanup")
for repo in repos:
    registry.delete(repo.repo_id)
print("✓ All test repositories deleted")

print("\n" + "=" * 60)
print("Example completed successfully!")
print("=" * 60)
