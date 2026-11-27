FROM mcr.microsoft.com/playwright:v1.52.0-noble

WORKDIR /app
COPY package.json ./
RUN npm install --omit=dev --no-audit --no-fund
COPY server.mjs ./

USER pwuser
ENV PORT=3003 \
    HOST=0.0.0.0 \
    MAX_CONCURRENT_PAGES=4 \
    BLOCK_MEDIA=true

EXPOSE 3003
CMD ["node", "server.mjs"]
