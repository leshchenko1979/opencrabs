# Log Rotation Setup Guide

## Overview
This guide explains how to set up and maintain automated log rotation on the VDS server using logrotate and cron jobs.

## Prerequisites
- SSH access to the VDS server (104.128.131.166) as root
- Ubuntu/Debian-based operating system
- Sufficient disk space for compressed logs during rotation

## Installation

### 1. Run the Setup Script
```bash
./scripts/setup/setup-logrotate.sh
```

This script will:
- Check if logrotate is installed (install if needed)
- Backup existing configurations
- Create logrotate configuration files for all services
- Create cron jobs for automated rotation
- Create monitoring and test scripts
- Test the configuration

### 2. Verify Installation
After the setup script completes, verify the installation:

```bash
# Check logrotate configuration
logrotate -d /etc/logrotate.conf

# Check cron jobs
ls -la /etc/cron.d/logrotate-*

# Test log rotation
/usr/local/sbin/test-logrotate.sh
```

## Configuration Files

### Logrotate Configurations
- `/etc/logrotate.d/docker-custom` - Docker log rotation (weekly, 4 weeks)
- `/etc/logrotate.d/system-custom` - System log rotation (daily, 7 days)
- `/etc/logrotate.d/application-custom` - Application log rotation (daily, 14 days)

### Scripts
- `/usr/local/sbin/logrotate-daily.sh` - Daily rotation script
- `/usr/local/sbin/logrotate-weekly.sh` - Weekly cleanup script
- `/usr/local/sbin/check-logrotate.sh` - Status monitoring script
- `/usr/local/sbin/test-logrotate.sh` - Testing script

### Cron Jobs
- `/etc/cron.d/logrotate-daily` - Runs daily at 2:00 AM
- `/etc/cron.d/logrotate-weekly` - Runs weekly on Sunday at 3:00 AM

## Log Retention Policies

| Log Type | Frequency | Retention | Location |
|----------|-----------|-----------|----------|
| Docker | Weekly | 4 weeks | /var/lib/docker/containers/ |
| System | Daily | 7 days | /var/log/ |
| Application | Daily | 14 days | /var/log/amnezia/, /opt/amnezia/logs/ |

## Monitoring

### Check Rotation Status
```bash
# Check today's rotation status
/usr/local/sbin/check-logrotate.sh

# View rotation logs
tail -f /var/log/logrotate-daily.log
tail -f /var/log/logrotate-weekly.log

# View rotation history
cat /var/log/logrotate-status
```

### Manual Rotation
```bash
# Force rotate all logs
logrotate -f /etc/logrotate.conf

# Force rotate specific configuration
logrotate -f /etc/logrotate.d/application-custom
```

## Troubleshooting

### Common Issues

1. **Logrotate not running**
   ```bash
   # Check cron service
   systemctl status cron
   
   # Check cron logs
   grep CRON /var/log/syslog
   ```

2. **Logs not rotating**
   ```bash
   # Check configuration syntax
   logrotate -d /etc/logrotate.conf
   
   # Check file permissions
   ls -la /var/log/
   ```

3. **Disk space still filling up**
   ```bash
   # Check retention policies
   grep rotate /etc/logrotate.d/*
   
   # Verify compression is working
   ls -la /var/log/*.gz
   ```

### Recovery Procedures

1. **Restore from backup**
   ```bash
   # Find backup directory (created during setup)
   ls -la /tmp/logrotate-backup-*
   
   # Restore configuration
   cp -r /tmp/logrotate-backup-*/logrotate.d/* /etc/logrotate.d/
   ```

2. **Manual rotation**
   ```bash
   # Force rotation
   logrotate -f /etc/logrotate.conf
   
   # Check results
   ls -la /var/log/
   ```

## Maintenance

### Monthly Tasks
1. Review log retention policies
2. Check disk space usage
3. Verify alerts are working
4. Review rotation logs for errors

### Quarterly Tasks
1. Update configuration if needed
2. Review and update documentation
3. Test disaster recovery procedures
4. Check for logrotate updates

## Integration with Existing Scripts

### Modified cleanup.sh
The cleanup.sh script has been modified to work with the new logrotate setup. It now:
- Checks logrotate status
- Uses logrotate for log rotation instead of manual truncation
- Preserves logrotate configurations during cleanup

### Enhanced diagnostics.sh
The diagnostics.sh script now includes:
- Logrotate installation check
- Configuration syntax validation
- Last rotation status
- Disk space analysis for logs

### Updated system-update.sh
The system-update.sh script now:
- Verifies logrotate configuration after updates
- Checks for logrotate package updates
- Ensures cron jobs are still active

## Email Alerts

To configure email alerts for log rotation failures:

1. Install and configure a mail service (postfix, sendmail, etc.)
2. Update the email address in the scripts:
   ```bash
   # Edit these files
   /usr/local/sbin/logrotate-daily.sh
   /usr/local/sbin/check-logrotate.sh
   
   # Change this line
   ALERT_EMAIL="admin@example.com"  # Configure this
   ```
3. Uncomment the mail commands in the scripts

## Customization

### Adding New Log Files
1. Create a new logrotate configuration file in `/etc/logrotate.d/`
2. Test the configuration with `logrotate -d`
3. Add the log file to the appropriate monitoring script if needed

### Changing Retention Policies
1. Edit the appropriate configuration file in `/etc/logrotate.d/`
2. Change the `rotate` value to the desired number of days/weeks
3. Test the configuration with `logrotate -d`

### Adjusting Rotation Schedule
1. Edit the cron job files in `/etc/cron.d/`
2. Modify the schedule as needed
3. Reload the cron service: `systemctl reload cron`

## Security Considerations

1. Log files may contain sensitive information
2. Ensure proper file permissions on log files
3. Consider encrypting logs if required by compliance
4. Regularly review who has access to log files

## Performance Impact

1. Log rotation is scheduled during low-traffic hours (2:00 AM)
2. Compression may temporarily increase CPU usage
3. Large log files may take longer to rotate
4. Monitor system load during rotation periods

## Compliance

1. Retention policies can be adjusted to meet compliance requirements
2. Consider legal requirements for log retention
3. Document any custom retention policies
4. Regularly audit log retention practices