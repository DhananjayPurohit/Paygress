# paygress/agent-sandbox image — generic compute sandbox for AI agents,
# CI runners, and map-reduce shards.
#
# Layer choices:
# - nikolaik/python-nodejs:python3.12-nodejs20 base. Already had this
#   reference in the agent-sandbox template as the minimum-viable
#   "Python + Node + git" surface; we layer on top instead of starting
#   from a leaner base because the cross-compatibility (apt, pip, npm
#   all available) outweighs ~500MB image size for the agent use case.
# - server.py is a self-contained stdlib HTTP server; no pip install
#   step in the build.
# - ENTRYPOINT runs the server. WORKSPACE / EXEC_USER / EXEC_PASS are
#   read from env at runtime — the provider injects them at spawn
#   time from the consumer's spawn request (ssh creds reused).
#
# Build (locally):
#   docker build -t ghcr.io/dhananjaypurohit/paygress-agent-sandbox:0.1.0 \
#                images/agent-sandbox
#
# CI publishes on tags `agent-sandbox-v*` via
# .github/workflows/agent-sandbox-image.yml.

FROM nikolaik/python-nodejs:python3.12-nodejs20

# /workspace is the contract surface — agents drop scripts here, the
# exec server defaults its CWD here, and the data_path on the
# template config points at the same path so a per-vmid Docker volume
# persists across container restarts on the same provider.
RUN mkdir -p /workspace && chmod 0777 /workspace

COPY server.py /usr/local/bin/paygress-exec
RUN chmod +x /usr/local/bin/paygress-exec

ENV WORKSPACE=/workspace \
    PYTHONUNBUFFERED=1 \
    NODE_ENV=production

EXPOSE 8080

ENTRYPOINT ["python3", "/usr/local/bin/paygress-exec"]
