export type ConnectionStatus =
  | 'disconnected'
  | 'connecting'
  | 'connected'
  | 'disconnecting'
  | 'error';

export type LogSource = 'system' | 'vpn' | 'installer' | 'process' | 'subscription' | 'settings';

export interface LogEntry {
  id: string;
  timestamp: number;
  message: string;
  level: 'info' | 'error' | 'warning' | 'success';
  source: LogSource;
}

export interface Server {
  id: string;
  name: string;
  flag?: string;
  ping?: number;
  protocol?: string;
  server?: string;
  port?: number;
  uuid?: string;
  password?: string;
  method?: string;
  sni?: string;
  flow?: string;
  publicKey?: string;
  privateKey?: string;
  transportType?: string;
  transportPath?: string;
  transportHost?: string;
  realityPbk?: string;
  realitySid?: string;
}

export interface Profile {
  id: string;
  name: string;
  url: string;
  lastUpdated?: number;
  servers: Server[];
  error?: string;
  trafficUsed?: number;
  trafficTotal?: number;
  trafficExpiry?: string;
}

export interface ConnectionInfo {
  id: string;
  host?: string;
  metadata?: {
    network?: string;
    type?: string;
    source_ip?: string;
    destination_ip?: string;
    destination_port?: string;
    process?: string;
    process_path?: string;
    host?: string;
  };
  rule?: string;
  chains?: string[];
  upload_total?: number;
  download_total?: number;
  start?: string;
}

export interface SpeedTestResult {
  delay?: number;
  mean_delay?: number;
}

export interface CustomCountry {
  code: string;
  name: string;
  flag: string;
}

export interface Settings {
  acceptTerms: boolean;
  routingMode: 'vpn' | 'system-proxy' | 'local-proxy';
  proxyPort: number;
  bypassRu: boolean;
  startWithWindows: boolean;
  autoConnectFastest: boolean;
  killSwitch: boolean;
  allowLan: boolean;
  customDns: boolean;
  dnsAddress: string;
  allowedCountries: string[];
  blockedCountries: string[];
  customCountries: CustomCountry[];
  allowedProtocols: string[];
  blockedProtocols: string[];
  language: 'en' | 'ru';
  autoUpdateInterval: number;
  antiDpi: boolean;
  enableMux: boolean;
  minimizeToTray: boolean;
}
