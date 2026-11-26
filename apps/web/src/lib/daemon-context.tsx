import { useQueryClient } from '@tanstack/react-query'
import { WakuClient } from '@waku/client'
import {
  createContext,
  useCallback,
  useContext,
  useEffect,
  useRef,
  useState,
  type ReactNode,
} from 'react'
import {
  clearStoredConnection,
  loadStoredConnection,
  storeConnection,
  validateConnectionConfig,
  type ConnectionConfig,
} from './connection'
import { translate, useI18n } from './i18n'

export type ConnectionPhase =
  | 'booting'
  | 'disconnected'
  | 'connecting'
  | 'connected'
  | 'error'

interface DaemonContextValue {
  client: WakuClient | null
  config: ConnectionConfig | null
  phase: ConnectionPhase
  error: string | null
  connections: number
  connect: (config: ConnectionConfig) => Promise<void>
  reconnect: () => Promise<void>
  disconnect: () => void
  forget: () => void
}

const DaemonContext = createContext<DaemonContextValue | null>(null)
const INITIAL_RECONNECT_DELAY_MS = 500
const MAX_RECONNECT_DELAY_MS = 15_000

export function DaemonProvider({ children }: { children: ReactNode }) {
  const { locale } = useI18n()
  const queryClient = useQueryClient()
  const [client, setClient] = useState<WakuClient | null>(null)
  const [config, setConfig] = useState<ConnectionConfig | null>(null)
  const [phase, setPhase] = useState<ConnectionPhase>('booting')
  const [error, setError] = useState<string | null>(null)
  const [connections, setConnections] = useState(0)
  const generation = useRef(0)
  const bootstrapped = useRef(false)
  const manualConnectInFlight = useRef(false)
  const phaseRef = useRef(phase)
  phaseRef.current = phase

  const open = useCallback(
    async (candidate: ConnectionConfig) => {
      const normalized = validateConnectionConfig(
        candidate,
        (key) => translate(locale, key),
      )
      const attempt = ++generation.current
      client?.disconnect()
      queryClient.clear()
      setPhase('connecting')
      setError(null)
      setConfig(normalized)

      const next = new WakuClient({
        address: normalized.address,
        token: normalized.token,
        clientKind: 'web',
      })
      setClient(next)
      try {
        await next.connect()
        if (generation.current !== attempt) {
          next.disconnect()
          return
        }
        storeConnection(normalized)
        setPhase('connected')
        setConnections((count) => count + 1)
      } catch (cause) {
        if (generation.current !== attempt) return
        const message = cause instanceof Error ? cause.message : String(cause)
        setError(message)
        setPhase('error')
        throw cause
      }
    },
    [client, locale, queryClient],
  )

  useEffect(() => {
    if (bootstrapped.current) return
    bootstrapped.current = true
    const stored = loadStoredConnection()
    if (!stored) {
      setPhase('disconnected')
      return
    }
    void open(stored).catch(() => {})
    // Storage is read once at browser startup.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [])

  useEffect(() => {
    if (!client) return
    const unsubscribeTaskState = client.subscribeTaskState(() => {
      if (!config) return
      void queryClient.invalidateQueries({
        queryKey: ['daemon', config.address, 'task-state'],
      })
    })
    let reconnectTimer: number | undefined
    let disposed = false
    let attempt = 0
    const activeGeneration = generation.current

    const scheduleReconnect = () => {
      if (disposed || reconnectTimer !== undefined) return
      if (generation.current !== activeGeneration || manualConnectInFlight.current) return
      const delay = Math.min(INITIAL_RECONNECT_DELAY_MS * 2 ** attempt, MAX_RECONNECT_DELAY_MS)
      attempt += 1
      reconnectTimer = window.setTimeout(() => {
        reconnectTimer = undefined
        if (disposed || generation.current !== activeGeneration) return
        if (manualConnectInFlight.current || client.connected) return
        void client
          .connect()
          .then(() => {
            if (disposed || generation.current !== activeGeneration) return
            attempt = 0
            setError(null)
            setPhase('connected')
            setConnections((count) => count + 1)
            void queryClient.invalidateQueries()
          })
          .catch((cause) => {
            if (disposed || generation.current !== activeGeneration) return
            setError(cause instanceof Error ? cause.message : translate(locale, 'web.daemon_connection_closed'))
            setPhase('error')
            scheduleReconnect()
          })
      }, delay)
    }

    const unsubscribeConnectionState = client.subscribeConnectionState((state) => {
      if (disposed || generation.current !== activeGeneration) return
      if (state === 'connected') {
        attempt = 0
        if (reconnectTimer !== undefined) {
          window.clearTimeout(reconnectTimer)
          reconnectTimer = undefined
        }
        return
      }
      if (state === 'disconnected' && !manualConnectInFlight.current && config) {
        setError(translate(locale, 'web.daemon_connection_closed'))
        setPhase('error')
        scheduleReconnect()
      }
    })

    if (!client.connected && phaseRef.current === 'error' && config) {
      scheduleReconnect()
    }

    return () => {
      disposed = true
      unsubscribeTaskState()
      unsubscribeConnectionState()
      if (reconnectTimer !== undefined) {
        window.clearTimeout(reconnectTimer)
      }
    }
  }, [client, config, locale, queryClient])

  const reconnect = useCallback(async () => {
    if (!client || !config) throw new Error(translate(locale, 'web.daemon_not_configured'))
    manualConnectInFlight.current = true
    setPhase('connecting')
    setError(null)
    try {
      await client.connect()
      setPhase('connected')
      setConnections((count) => count + 1)
      void queryClient.invalidateQueries()
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : String(cause))
      setPhase('error')
      throw cause
    } finally {
      manualConnectInFlight.current = false
    }
  }, [client, config, locale, queryClient])

  const disconnect = useCallback(() => {
    ++generation.current
    client?.disconnect()
    setPhase('disconnected')
    setError(null)
    setConnections(0)
    queryClient.clear()
  }, [client, queryClient])

  const forget = useCallback(() => {
    disconnect()
    clearStoredConnection()
    setClient(null)
    setConfig(null)
  }, [disconnect])

  useEffect(() => () => client?.disconnect(), [client])

  const value: DaemonContextValue = {
    client,
    config,
    phase,
    error,
    connections,
    connect: open,
    reconnect,
    disconnect,
    forget,
  }

  return <DaemonContext.Provider value={value}>{children}</DaemonContext.Provider>
}

export function useDaemon() {
  const context = useContext(DaemonContext)
  if (!context) throw new Error('useDaemon must be used inside DaemonProvider')
  return context
}
