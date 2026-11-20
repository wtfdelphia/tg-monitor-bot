DO $$
BEGIN
  EXECUTE format('GRANT CONNECT ON DATABASE %I TO shared_cleanup', current_database());
END
$$;
