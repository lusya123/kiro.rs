import { useState, useEffect } from 'react'
import { useQuery, useMutation, useQueryClient } from '@tanstack/react-query'
import { toast } from 'sonner'
import { Shield, RefreshCw, AlertTriangle, CheckCircle2 } from 'lucide-react'
import {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogTitle,
  DialogDescription,
  DialogFooter,
} from '@/components/ui/dialog'
import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'
import { Switch } from '@/components/ui/switch'
import { Badge } from '@/components/ui/badge'
import { getSidecarStatus, getSidecarConfig, updateSidecarConfig } from '@/api/sidecar'
import { extractErrorMessage } from '@/lib/utils'

interface SidecarSettingsDialogProps {
  open: boolean
  onOpenChange: (open: boolean) => void
}

export function SidecarSettingsDialog({ open, onOpenChange }: SidecarSettingsDialogProps) {
  const queryClient = useQueryClient()

  const { data: status, isLoading: loadingStatus } = useQuery({
    queryKey: ['sidecar', 'status'],
    queryFn: getSidecarStatus,
    enabled: open,
    refetchInterval: open ? 5000 : false, // 打开时每 5s 刷新状态
  })

  const { data: config, isLoading: loadingConfig } = useQuery({
    queryKey: ['sidecar', 'config'],
    queryFn: getSidecarConfig,
    enabled: open,
  })

  // 表单本地状态
  // 注：upstreamProxy（上游代理）不在 UI 暴露——属于"换出口 IP"的独立功能，
  // 跟 TLS 指纹伪装是两个独立维度。需要的用户直接改 config.json 的 tlsSidecarProxyUrl。
  const [enabled, setEnabled] = useState(true)
  const [port, setPort] = useState('9090')
  const [binaryPath, setBinaryPath] = useState('')

  // 表单同步：在「打开 dialog」或「config 变化」时从 server 重置
  // 关键：包含 open 依赖，避免「打开 → 编辑 → 关闭不保存 → 重开」时残留本地 state
  useEffect(() => {
    if (open && config) {
      setEnabled(config.enabled)
      setPort(String(config.port))
      setBinaryPath(config.binaryPath ?? '')
    }
  }, [open, config])

  const updateMutation = useMutation({
    mutationFn: updateSidecarConfig,
    onSuccess: (resp) => {
      if (resp.requiresRestart) {
        toast.warning(resp.message, { duration: 6000 })
      } else {
        toast.success(resp.message)
      }
      queryClient.invalidateQueries({ queryKey: ['sidecar'] })
    },
    onError: (err) => {
      toast.error(`保存失败: ${extractErrorMessage(err)}`)
    },
  })

  const handleSave = () => {
    const portNum = parseInt(port, 10)
    if (isNaN(portNum) || portNum < 1 || portNum > 65535) {
      toast.error('端口必须是 1-65535 之间的整数')
      return
    }
    // 注：不传 upstreamProxy 字段，PUT 处理器会保留 server 端原值
    updateMutation.mutate({
      enabled,
      port: portNum,
      binaryPath: binaryPath.trim(),
    })
  }

  // 状态指示灯
  const statusBadge = () => {
    if (!status) return null
    if (!status.enabled) {
      return (
        <Badge variant="outline" className="gap-1">
          <span className="h-2 w-2 rounded-full bg-gray-400" />
          已禁用
        </Badge>
      )
    }
    if (status.running && status.lastHealthOk) {
      return (
        <Badge variant="outline" className="gap-1 border-green-500/50 text-green-600 dark:text-green-400">
          <span className="h-2 w-2 rounded-full bg-green-500 animate-pulse" />
          运行中
        </Badge>
      )
    }
    return (
      <Badge variant="outline" className="gap-1 border-red-500/50 text-red-600 dark:text-red-400">
        <span className="h-2 w-2 rounded-full bg-red-500" />
        未就绪
      </Badge>
    )
  }

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="sm:max-w-lg">
        <DialogHeader>
          <DialogTitle className="flex items-center gap-2">
            <Shield className="h-5 w-5" />
            TLS Sidecar 设置
          </DialogTitle>
          <DialogDescription>
            通过 Go uTLS 子进程伪装 Chrome TLS 指纹，避免上游基于 JA3/JA4 封号
          </DialogDescription>
        </DialogHeader>

        {loadingStatus || loadingConfig ? (
          <div className="flex items-center justify-center py-8">
            <RefreshCw className="h-6 w-6 animate-spin text-muted-foreground" />
          </div>
        ) : (
          <div className="space-y-5">
            {/* 实时状态卡片 */}
            <div className="rounded-lg border p-4 space-y-2 bg-muted/30">
              <div className="flex items-center justify-between">
                <span className="text-sm font-medium">运行状态</span>
                {statusBadge()}
              </div>
              {status?.enabled && (
                <div className="grid grid-cols-2 gap-x-4 gap-y-1 text-xs text-muted-foreground">
                  <div>端口: <span className="font-mono">{status.port}</span></div>
                  <div>累计重启: <span className="font-mono">{status.totalRestarts}</span></div>
                  {status.binaryPath && (
                    <div className="col-span-2 truncate">
                      二进制: <span className="font-mono">{status.binaryPath}</span>
                    </div>
                  )}
                  {status.lastHealthCheck && (
                    <div className="col-span-2">
                      上次健康检查: <span className="font-mono">
                        {new Date(status.lastHealthCheck).toLocaleString('zh-CN')}
                      </span>
                      {status.lastHealthOk
                        ? <CheckCircle2 className="inline h-3 w-3 ml-1 text-green-500" />
                        : <AlertTriangle className="inline h-3 w-3 ml-1 text-red-500" />}
                    </div>
                  )}
                </div>
              )}
            </div>

            {/* 启用开关 */}
            <div className="flex items-center justify-between">
              <div className="space-y-0.5">
                <div className="text-sm font-medium">启用 TLS Sidecar</div>
                <div className="text-xs text-muted-foreground">
                  关闭后将使用原生 rustls 指纹，存在被识别封号风险
                </div>
              </div>
              <Switch checked={enabled} onCheckedChange={setEnabled} />
            </div>

            {/* 端口 */}
            <div className="space-y-1.5">
              <label className="text-sm font-medium">监听端口</label>
              <Input
                type="number"
                value={port}
                onChange={(e) => setPort(e.target.value)}
                placeholder="9090"
                disabled={!enabled}
              />
              <p className="text-xs text-muted-foreground">需重启进程生效</p>
            </div>

            {/* 二进制路径 */}
            <div className="space-y-1.5">
              <label className="text-sm font-medium">二进制路径（可选）</label>
              <Input
                value={binaryPath}
                onChange={(e) => setBinaryPath(e.target.value)}
                placeholder="留空自动查找：/app/tls-sidecar 或 ./tls-sidecar/tls-sidecar"
                disabled={!enabled}
              />
              <p className="text-xs text-muted-foreground">需重启进程生效</p>
            </div>
          </div>
        )}

        <DialogFooter>
          <Button variant="outline" onClick={() => onOpenChange(false)}>
            关闭
          </Button>
          <Button onClick={handleSave} disabled={updateMutation.isPending}>
            {updateMutation.isPending ? (
              <><RefreshCw className="h-4 w-4 mr-2 animate-spin" />保存中</>
            ) : '保存'}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  )
}
