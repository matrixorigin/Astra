-- MatrixOne RBAC Initialization for mo-dev-agent
-- This script creates custom roles and grants appropriate privileges

-- Create custom roles for agent system
CREATE ROLE IF NOT EXISTS mo_agent_admin;
CREATE ROLE IF NOT EXISTS mo_agent_user;

-- Grant privileges to mo_agent_admin
-- Account-level privileges
GRANT CREATE DATABASE, DROP DATABASE, SHOW DATABASES ON account * TO mo_agent_admin;
GRANT CREATE USER, DROP USER, ALTER USER ON account * TO mo_agent_admin;
GRANT CREATE ROLE, DROP ROLE ON account * TO mo_agent_admin;

-- Database-level privileges for agent_config
GRANT ALL ON database agent_config TO mo_agent_admin;

-- Grant privileges to mo_agent_user
-- Minimal account-level privileges
GRANT CONNECT ON account * TO mo_agent_user;
GRANT SHOW DATABASES ON account * TO mo_agent_user;

-- Read access to agent_config tables
GRANT SELECT ON table agent_config.* TO mo_agent_user;

-- Write access to user-scoped skills only (enforced by application logic)
GRANT INSERT, UPDATE, DELETE ON table agent_config.skills_registry TO mo_agent_user;

-- Display created roles
SELECT 'Custom roles created successfully' AS status;

-- Example: Grant role to user
-- GRANT mo_agent_user TO alice;
-- GRANT mo_agent_admin TO admin;
