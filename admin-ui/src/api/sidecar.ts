import axios from 'axios'
import { storage } from '@/lib/storage'
import type {
  SidecarStatus,
  SidecarConfig,
  SidecarConfigUpdateRequest,
  SidecarConfigUpdateResponse,
} from '@/types/api'

const api = axios.create({
  baseURL: '/api/admin',
  headers: { 'Content-Type': 'application/json' },
})

api.interceptors.request.use((config) => {
  const apiKey = storage.getApiKey()
  if (apiKey) {
    config.headers['x-api-key'] = apiKey
  }
  return config
})

export async function getSidecarStatus(): Promise<SidecarStatus> {
  const { data } = await api.get<SidecarStatus>('/sidecar/status')
  return data
}

export async function getSidecarConfig(): Promise<SidecarConfig> {
  const { data } = await api.get<SidecarConfig>('/sidecar/config')
  return data
}

export async function updateSidecarConfig(
  req: SidecarConfigUpdateRequest
): Promise<SidecarConfigUpdateResponse> {
  const { data } = await api.put<SidecarConfigUpdateResponse>('/sidecar/config', req)
  return data
}
