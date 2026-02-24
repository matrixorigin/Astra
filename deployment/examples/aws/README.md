# AWS Deployment Example

Deploy mo-agent to AWS using ECS (Elastic Container Service).

## Prerequisites

- AWS CLI configured
- Docker installed
- ECR repository created

## Quick Start

```bash
# 1. Build and push image
./build-and-push.sh

# 2. Create ECS cluster
aws ecs create-cluster --cluster-name mo-agent

# 3. Register task definition
aws ecs register-task-definition --cli-input-json file://task-definition.json

# 4. Create service
aws ecs create-service \
  --cluster mo-agent \
  --service-name mo-agent-api \
  --task-definition mo-agent \
  --desired-count 3 \
  --launch-type FARGATE \
  --network-configuration "awsvpcConfiguration={subnets=[subnet-xxx],securityGroups=[sg-xxx],assignPublicIp=ENABLED}"
```

## Architecture

```
┌─────────────────────────────────────────────────────────┐
│                     AWS Cloud                            │
│                                                          │
│  ┌────────────┐      ┌──────────────────────────┐      │
│  │    ALB     │─────▶│   ECS Service (Fargate)  │      │
│  │            │      │   ┌──────┐  ┌──────┐     │      │
│  │  Port 443  │      │   │ API  │  │ API  │     │      │
│  └────────────┘      │   └──────┘  └──────┘     │      │
│                      └──────────────────────────┘      │
│                                                          │
│  ┌────────────┐      ┌──────────────────────────┐      │
│  │    RDS     │      │      ElastiCache         │      │
│  │ (MatrixOne)│      │       (Redis)            │      │
│  └────────────┘      └──────────────────────────┘      │
│                                                          │
│  ┌────────────┐      ┌──────────────────────────┐      │
│  │ Secrets    │      │      CloudWatch          │      │
│  │ Manager    │      │      (Monitoring)        │      │
│  └────────────┘      └──────────────────────────┘      │
└─────────────────────────────────────────────────────────┘
```

## Components

### 1. Container Registry (ECR)

```bash
# Create ECR repository
aws ecr create-repository --repository-name mo-agent

# Get login command
aws ecr get-login-password --region us-east-1 | \
  docker login --username AWS --password-stdin <account>.dkr.ecr.us-east-1.amazonaws.com
```

### 2. Database (RDS or Self-Managed)

**Option A: Self-managed MatrixOne on EC2**
```bash
# Launch EC2 instance
aws ec2 run-instances \
  --image-id ami-xxx \
  --instance-type t3.large \
  --key-name my-key \
  --security-group-ids sg-xxx \
  --subnet-id subnet-xxx

# Install MatrixOne
ssh ec2-user@<instance-ip>
docker run -d -p 6001:6001 matrixorigin/matrixone:latest
```

**Option B: RDS MySQL (compatible)**
```bash
# Create RDS instance
aws rds create-db-instance \
  --db-instance-identifier mo-agent-db \
  --db-instance-class db.t3.medium \
  --engine mysql \
  --master-username admin \
  --master-user-password <password> \
  --allocated-storage 100
```

### 3. Cache (ElastiCache)

```bash
# Create Redis cluster
aws elasticache create-cache-cluster \
  --cache-cluster-id mo-agent-redis \
  --cache-node-type cache.t3.micro \
  --engine redis \
  --num-cache-nodes 1
```

### 4. Secrets (Secrets Manager)

```bash
# Store secrets
aws secretsmanager create-secret \
  --name mo-agent/prod/token-key \
  --secret-string "your-token-encryption-key"

aws secretsmanager create-secret \
  --name mo-agent/prod/jwt-secret \
  --secret-string "your-jwt-secret"

aws secretsmanager create-secret \
  --name mo-agent/prod/openai-key \
  --secret-string "your-openai-api-key"
```

### 5. Load Balancer (ALB)

```bash
# Create Application Load Balancer
aws elbv2 create-load-balancer \
  --name mo-agent-alb \
  --subnets subnet-xxx subnet-yyy \
  --security-groups sg-xxx

# Create target group
aws elbv2 create-target-group \
  --name mo-agent-targets \
  --protocol HTTP \
  --port 8000 \
  --vpc-id vpc-xxx \
  --health-check-path /health
```

## Configuration

### Environment Variables

Store in ECS task definition or use Secrets Manager:

```json
{
  "environment": [
    {"name": "MATRIXONE_HOST", "value": "db.internal"},
    {"name": "REDIS_HOST", "value": "redis.internal"}
  ],
  "secrets": [
    {"name": "TOKEN_ENCRYPTION_KEY", "valueFrom": "arn:aws:secretsmanager:..."},
    {"name": "JWT_SECRET_KEY", "valueFrom": "arn:aws:secretsmanager:..."}
  ]
}
```

## Monitoring

### CloudWatch

```bash
# View logs
aws logs tail /ecs/mo-agent --follow

# Create alarm
aws cloudwatch put-metric-alarm \
  --alarm-name mo-agent-high-cpu \
  --comparison-operator GreaterThanThreshold \
  --evaluation-periods 2 \
  --metric-name CPUUtilization \
  --namespace AWS/ECS \
  --period 300 \
  --statistic Average \
  --threshold 80
```

## Scaling

### Auto Scaling

```bash
# Register scalable target
aws application-autoscaling register-scalable-target \
  --service-namespace ecs \
  --resource-id service/mo-agent/mo-agent-api \
  --scalable-dimension ecs:service:DesiredCount \
  --min-capacity 2 \
  --max-capacity 10

# Create scaling policy
aws application-autoscaling put-scaling-policy \
  --service-namespace ecs \
  --resource-id service/mo-agent/mo-agent-api \
  --scalable-dimension ecs:service:DesiredCount \
  --policy-name cpu-scaling \
  --policy-type TargetTrackingScaling \
  --target-tracking-scaling-policy-configuration file://scaling-policy.json
```

## Cost Optimization

- Use Fargate Spot for non-critical workloads
- Enable RDS auto-scaling
- Use ElastiCache reserved nodes
- Set up CloudWatch alarms for cost anomalies

## See Also

- [task-definition.json](task-definition.json) - ECS task definition
- [build-and-push.sh](build-and-push.sh) - Build and push script
- [scaling-policy.json](scaling-policy.json) - Auto-scaling policy
