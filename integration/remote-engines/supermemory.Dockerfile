FROM node:22-bookworm-slim

RUN apt-get update \
    && apt-get install --yes --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*

RUN npm install --global supermemory@4

ENV PORT=6767
EXPOSE 6767

ENTRYPOINT ["supermemory", "local"]
