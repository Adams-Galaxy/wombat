# Security Policy

## Supported versions

Wombat has not reached a stable release. Security fixes are made on the current
`main` branch; historical pre-release versions are not maintained.

## Reporting a vulnerability

Do not open a public issue for a vulnerability that could expose user files,
credentials, command execution, or deployment integrity. Use GitHub's private
security-advisory reporting for this repository. Include the affected command,
platform, reproduction steps, impact, and any proposed mitigation.

## Trust boundary

Wombat configuration Lua, tasks, scripts, and custom providers are trusted
programs. Running an untrusted Wombat repository is equivalent to running other
untrusted local code. Reports should distinguish vulnerabilities that escape a
documented Wombat boundary from behavior explicitly authorized by repository
code.
