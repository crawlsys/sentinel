FROM python:3.12-slim-bookworm
LABEL org.opencontainers.image.source=https://github.com/crawlsys/sentinel

WORKDIR /app

COPY requirements.txt .
RUN pip install --no-cache-dir -r requirements.txt

COPY sentinel.py .

USER 1000
CMD ["python3", "-u", "sentinel.py"]
