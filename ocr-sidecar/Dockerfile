FROM node:22-bookworm-slim

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates poppler-utils \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY package.json ./
RUN npm install --omit=dev --no-audit --no-fund \
    && npx lit models pull ocria-nano
COPY server.mjs ./

ENV PORT=3004 \
    HOST=0.0.0.0 \
    OCR_BACKEND=ocria-nano \
    LIT_BIN=/app/node_modules/.bin/lit \
    MAX_CONCURRENT_OCR=2

EXPOSE 3004
CMD ["node", "server.mjs"]
