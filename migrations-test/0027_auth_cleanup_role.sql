DO $$
BEGIN
  EXECUTE format('GRANT CONNECT ON DATABASE %I TO auth_cleanup', current_database());
END
$$;
