FROM a
HEALTHCHECK NONE
HEALTHCHECK --interval=5s --timeout=1m30s --start-period=0 --start-interval=2.5s --retries=3 CMD curl -f http://x/
HEALTHCHECK CMD ["/bin/check", "--fast"]
HEALTHCHECK --retries=0 --interval=1ms cmd true
healthcheck --interval=1h CMD-SHELL x
