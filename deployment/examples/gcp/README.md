# GCP Deployment Example

Deploy astra to Google Cloud Platform using Cloud Run or GKE.

## Option 1: Cloud Run (Serverless)

### Quick Start

```bash
# 1. Build and push to GCR
gcloud builds submit --tag gcr.io/PROJECT_ID/astra

# 2. Deploy to Cloud Run
gcloud run deploy astra \
  --image gcr.io/PROJECT_ID/astra \
  --platform managed \
  --region us-central1 \
  --allow-unauthenticated \
  --set-env-vars="MATRIXONE_HOST=DB_HOST" \
  --set-secrets="ASTRA_TOKEN_ENCRYPTION_KEY=token-key:latest,ASTRA_JWT_SECRET=jwt-secret:latest"
```

### Architecture

```
┌─────────────────────────────────────────────────────────┐
│                    Google Cloud                          │
│                                                          │
│  ┌────────────┐      ┌──────────────────────────┐      │
│  │ Cloud Load │─────▶│      Cloud Run           │      │
│  │ Balancing  │      │   (Auto-scaling)         │      │
│  └────────────┘      └──────────────────────────┘      │
│                                                          │
│  ┌────────────┐      ┌──────────────────────────┐      │
│  │ Cloud SQL  │      │   Memorystore (Redis)    │      │
│  └────────────┘      └──────────────────────────┘      │
│                                                          │
│  ┌────────────┐      ┌──────────────────────────┐      │
│  │ Secret     │      │   Cloud Monitoring       │      │
│  │ Manager    │      │                          │      │
│  └────────────┘      └──────────────────────────┘      │
└─────────────────────────────────────────────────────────┘
```

## Option 2: GKE (Kubernetes)

### Quick Start

```bash
# 1. Create GKE cluster
gcloud container clusters create astra \
  --num-nodes=3 \
  --region=us-central1

# 2. Get credentials
gcloud container clusters get-credentials astra --region=us-central1

# 3. Deploy with Helm
helm install astra ../../kubernetes/chart
```

## Components

### 1. Container Registry (GCR)

```bash
# Build and push
gcloud builds submit --tag gcr.io/PROJECT_ID/astra

# Or use Docker
docker build -t gcr.io/PROJECT_ID/astra .
docker push gcr.io/PROJECT_ID/astra
```

### 2. Database (Cloud SQL)

```bash
# Create Cloud SQL instance
gcloud sql instances create astra-db \
  --database-version=MYSQL_8_0 \
  --tier=db-n1-standard-2 \
  --region=us-central1
```

### 3. Cache (Memorystore)

```bash
# Create Redis instance
gcloud redis instances create astra-redis \
  --size=1 \
  --region=us-central1 \
  --redis-version=redis_6_x
```

### 4. Secrets (Secret Manager)

```bash
# Create secrets
echo -n "your-token-key" | gcloud secrets create token-key --data-file=-
echo -n "your-jwt-secret" | gcloud secrets create jwt-secret --data-file=-
echo -n "your-openai-key" | gcloud secrets create openai-key --data-file=-
```

## Monitoring

```bash
# View logs
gcloud logging read "resource.type=cloud_run_revision AND resource.labels.service_name=astra"

# Create alert
gcloud alpha monitoring policies create \
  --notification-channels=CHANNEL_ID \
  --display-name="High Error Rate" \
  --condition-display-name="Error rate > 5%" \
  --condition-threshold-value=0.05
```

## Scaling

Cloud Run auto-scales by default. For GKE:

```bash
# Horizontal Pod Autoscaler
kubectl autoscale deployment astra-api \
  --cpu-percent=70 \
  --min=2 \
  --max=10
```

## See Also

- [cloudbuild.yaml](cloudbuild.yaml) - Cloud Build configuration
- [app.yaml](app.yaml) - App Engine configuration (alternative)
