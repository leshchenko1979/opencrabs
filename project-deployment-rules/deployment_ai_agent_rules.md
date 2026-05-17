# Deployment AI Agent Rules

## Core Principles

### 1. **Security First**

- **Source File Removal**: Always remove source files after deployment for web services (security)
- **Environment Variables**: Never expose sensitive credentials in code or logs
- **SSL/TLS**: Always use proper certificates (Let's Encrypt preferred over self-signed)
- **Access Control**: Implement proper authentication and authorization

### 2. **Infrastructure Standards**

- **Traefik Labels**: Use `certResolver: le` for all web services (Let's Encrypt)
- **Sablier Integration**: Use `sablier.managed=true` for scale-to-zero services
- **Resource Limits**: Set CPU/memory limits in all Docker deployments
- **Network Security**: Use internal networks, avoid exposing unnecessary ports

### 3. **Deployment Best Practices**

- **Tarball Transfer**: Use tarball deployment for optimal performance (single scp operation)
- **SSH Optimization**: Implement connection multiplexing for faster deployments
- **Health Verification**: Always wait for services to be healthy after deployment
- **Rollback Ready**: Maintain backup strategies and quick rollback capabilities

### 4. **Configuration Management**

- **Pydantic-Settings**: Use standardized configuration patterns
- **Environment Separation**: Support .env.local/.env.prod for different environments
- **Validation**: Implement proper field validation and error handling
- **Caching**: Use @lru_cache() for configuration objects

### 5. **Logging Standards**

- **Logfire Integration**: Implement centralized logging where possible
- **File Logging**: Provide local file logging as backup to cloud services
- **Console Logging**: Configure appropriate levels (INFO for production)
- **Test-Aware**: Skip external services during testing

### 6. **Testing Requirements**

- **Pytest Standards**: Use `--maxfail=1 --exitfirst --last-failed -q`
- **Test Isolation**: Ensure tests don't interfere with production systems
- **Coverage**: Aim for comprehensive test coverage
- **CI/CD Integration**: Support automated testing in deployment pipelines

### 7. **Monitoring & Observability**

- **Health Endpoints**: Implement proper `/health` endpoints
- **Metrics**: Expose Prometheus-compatible metrics
- **Alerting**: Configure appropriate alerting thresholds
- **Dashboards**: Provide Grafana dashboards for key metrics

## Service-Specific Rules

### Web Applications (Sablier-Managed)

- **Scale-to-Zero**: Use Sablier for cost optimization
- **Health Checks**: Implement comprehensive health verification
- **Resource Optimization**: Fine-tune memory reservations and limits
- **Load Balancing**: Configure proper service discovery

### Bot Applications

- **Webhook Security**: Validate incoming webhook requests
- **Rate Limiting**: Implement appropriate rate limiting
- **Error Handling**: Robust error handling and recovery
- **Background Jobs**: Proper job queue management

### Cron Jobs

- **Timezone Handling**: Use Europe/Moscow timezone consistently
- **Cron Backup**: Always backup existing crontab before modification
- **Job Scheduling**: Implement proper job scheduling and overlap prevention
- **Log Rotation**: Configure logrotate for long-running jobs

## Quality Assurance

### Pre-Deployment Checks

- SSL certificates valid and not expiring soon
- All environment variables properly configured
- Health endpoints responding correctly
- Resource limits set appropriately
- Backup strategies in place

### Post-Deployment Verification

- Services start successfully
- Health checks pass
- Logs show no critical errors
- External connectivity works
- Monitoring systems receive data

## Emergency Procedures

### Rollback Strategy

1. **Quick Rollback**: Maintain previous working versions
2. **Gradual Rollback**: Scale down new version, scale up old version
3. **Data Integrity**: Ensure database migrations are reversible
4. **Communication**: Notify stakeholders of rollback

### Incident Response

1. **Assessment**: Quickly assess impact and scope
2. **Containment**: Isolate affected systems
3. **Recovery**: Execute rollback or fix procedures
4. **Post-Mortem**: Document lessons learned

## Documentation Requirements

### Deployment Documentation

- **Step-by-Step**: Clear deployment instructions
- **Prerequisites**: List all required dependencies
- **Troubleshooting**: Common issues and solutions
- **Monitoring**: How to monitor deployment success

### Service Documentation

- **API Endpoints**: Document all public APIs
- **Configuration**: All configurable options
- **Dependencies**: External service requirements
- **Scaling**: Horizontal and vertical scaling guidelines

## Tool-Specific Rules

### Docker

- **Multi-stage Builds**: Use for optimal image size
- **Security Scanning**: Scan images for vulnerabilities
- **Labels**: Use standardized labeling conventions
- **Networks**: Proper network isolation

### Traefik

- **Router Labels**: Consistent labeling patterns
- **Middleware**: Appropriate security middleware
- **TLS Options**: Always use certResolver: le
- **Load Balancing**: Configure based on service needs

### Monitoring Stack

- **Grafana**: Standardized dashboard layouts
- **Prometheus**: Proper metric naming conventions
- **Loki**: Structured logging formats
- **Alerting**: Clear alert definitions and thresholds

## Performance Optimization

### Application Level

- **Caching**: Implement appropriate caching strategies
- **Database Optimization**: Query optimization and indexing
- **Async Processing**: Use async patterns where beneficial
- **Memory Management**: Monitor and optimize memory usage

### Infrastructure Level

- **Resource Allocation**: Right-size CPU and memory
- **Network Optimization**: Minimize latency and optimize throughput
- **Storage**: Choose appropriate storage solutions
- **CDN**: Use CDN for static assets when applicable

## Compliance & Security

### Security Standards

- **OWASP Top 10**: Address common web vulnerabilities
- **Data Protection**: Implement proper data handling
- **Access Control**: Role-based access control
- **Audit Logging**: Comprehensive audit trails

### Compliance Requirements

- **Data Privacy**: GDPR/CCPA compliance where applicable
- **Industry Standards**: Relevant industry certifications
- **Regulatory Requirements**: Domain-specific regulations
- **Internal Policies**: Company security policies

## Maintenance & Updates

### Regular Tasks

- **Certificate Renewal**: Monitor SSL certificate expiration
- **Security Updates**: Apply security patches promptly
- **Performance Monitoring**: Regular performance reviews
- **Dependency Updates**: Keep dependencies current

### Backup & Recovery

- **Backup Strategy**: Regular automated backups
- **Recovery Testing**: Test backup restoration
- **Disaster Recovery**: Document and test DR procedures
- **Business Continuity**: Ensure service availability

## Communication & Collaboration

### Team Coordination

- **Deployment Windows**: Scheduled maintenance windows
- **Change Management**: Document all changes
- **Incident Reporting**: Clear escalation procedures
- **Knowledge Sharing**: Document lessons learned

### Stakeholder Communication

- **Status Updates**: Regular deployment status reports
- **Incident Communication**: Clear incident notifications
- **Change Notifications**: Upcoming change communications
- **Training**: Keep team members trained on procedures